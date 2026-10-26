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
//! exits without touching the layout files, which the new one owns now
//! (on macOS it stays as a process that only waits; see `hold`).
//!
//! Left behind: panes that hold a channel rather than a pty (ssh-domain
//! and tmux-domain panes) and panes whose process already exited. They
//! are pruned from the topology the new server gets, and end with the old
//! process. Also left behind, by design: bytes of a partial escape
//! sequence at the pause boundary, and output held by an unfinished
//! synchronized-output frame (the next frame redraws).
//!
//! Windows passes no descriptors over a socket. There the old server
//! duplicates each handle into the successor itself -- a pane's pseudo
//! console (its pipes, signal pipe, reference and console host) and its
//! process, and the pid file -- and sends the numbers they have there
//! (`Successor`). The listening socket does not travel: the successor binds
//! it once the old server has exited. Everything else is the same.

use anyhow::{anyhow, bail, Context};
use std::convert::TryFrom;
use thinkterm_proto::PaneNode;
use mux::domain::DomainId;
use mux::localpane::LocalPane;
use mux::pane::{Pane, PaneId};
use mux::tab::{Tab, TabId};
use mux::window::{Window, WindowId};
use mux::{Mux, PausedReader};
#[cfg(unix)]
use passfd::FdPassingExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::AtomicI32;
#[cfg(windows)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
#[cfg(windows)]
use wezterm_uds::{UnixListener, UnixStream};
use wezterm_term::{TerminalSize, TerminalSnapshot};

/// What travels with a pane besides its messages: the pty descriptor, or on
/// Windows the pseudo console's handles (`ConPtyHandoff::into_handles`)
/// followed by the process's.
#[cfg(unix)]
type PaneObjects = OwnedFd;
#[cfg(windows)]
type PaneObjects = Vec<OwnedHandle>;

/// The pid file as the successor receives it.
#[cfg(unix)]
pub type PidFile = OwnedFd;
#[cfg(windows)]
pub type PidFile = OwnedHandle;

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

/// macOS refuses a descriptor at once (EMSGSIZE) while the stream has less
/// room left than the descriptor takes, where a write would wait for the
/// reader -- and a pane's message can end just short of filling it. Wait
/// for the reader here too, as long as a step may take.
#[cfg(unix)]
fn send_fd(stream: &UnixStream, fd: RawFd) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut tries = 1;
    loop {
        match stream.as_raw_fd().send_fd(fd) {
            // A signal: with a send timeout set, the call is not restarted.
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err)
                if err.raw_os_error() == Some(libc::EMSGSIZE)
                    && started.elapsed() < STEP_TIMEOUT =>
            {
                std::thread::sleep(Duration::from_millis(1));
                tries += 1;
            }
            Ok(()) => {
                if tries > 1 {
                    log::info!(
                        "a descriptor waited {:?} for room in the handoff stream ({tries} tries)",
                        started.elapsed()
                    );
                }
                return Ok(());
            }
            Err(err) if tries > 1 => {
                return Err(err).with_context(|| {
                    format!(
                        "passing a file descriptor, after waiting {:?} for the reader to make room",
                        started.elapsed()
                    )
                });
            }
            Err(err) => return Err(err).context("passing a file descriptor"),
        }
    }
}

#[cfg(unix)]
fn recv_owned_fd(stream: &UnixStream) -> anyhow::Result<OwnedFd> {
    let fd = stream
        .as_raw_fd()
        .recv_fd()
        .context("receiving a file descriptor")?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The successor's process, open to receive handles, on Windows.
#[cfg(windows)]
struct Successor(OwnedHandle);

#[cfg(windows)]
impl Successor {
    /// The process at the other end of `stream`, if this server may give
    /// it its consoles: a mux server running as the same user at the same
    /// integrity level. Anything that can reach the handoff socket can
    /// already talk to this server, but the handles of an elevated server's
    /// panes are elevated shells, and must not reach a process below it.
    fn of(stream: &UnixStream) -> anyhow::Result<Self> {
        use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcess};
        use winapi::um::winnt::{PROCESS_DUP_HANDLE, PROCESS_QUERY_LIMITED_INFORMATION};
        let pid = peer_pid(stream)?;
        let handle = unsafe {
            OpenProcess(PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)
        };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("opening the successor, pid {pid}"));
        }
        let process = unsafe { OwnedHandle::from_raw_handle(handle as _) };
        if !mux::session_server::is_mux_server_process(pid) {
            bail!("the successor (pid {pid}) is not a mux server");
        }
        let ours = token_identity(unsafe { GetCurrentProcess() } as _)?;
        let theirs = token_identity(process.as_raw_handle())?;
        if ours != theirs {
            bail!(
                "the successor (pid {pid}) runs as another user or at another integrity level \
                 (an elevated server and one that is not)"
            );
        }
        Ok(Self(process))
    }

    /// Duplicate `handles` into the successor, then send the numbers they
    /// have there: a count, then each as a little-endian u64.
    fn send(&self, stream: &mut UnixStream, handles: &[RawHandle]) -> anyhow::Result<()> {
        use winapi::um::handleapi::DuplicateHandle;
        use winapi::um::processthreadsapi::GetCurrentProcess;
        use winapi::um::winnt::DUPLICATE_SAME_ACCESS;
        let mut sent = Vec::with_capacity(handles.len());
        for &handle in handles {
            let mut there = std::ptr::null_mut();
            let ok = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    handle as _,
                    self.0.as_raw_handle() as _,
                    &mut there,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            if ok == 0 {
                let err = std::io::Error::last_os_error();
                self.close(&sent);
                return Err(err).context("duplicating a handle into the successor");
            }
            sent.push(there as usize as u64);
        }
        let mut frame = Vec::with_capacity(4 + 8 * sent.len());
        frame.extend_from_slice(&(sent.len() as u32).to_le_bytes());
        for value in &sent {
            frame.extend_from_slice(&value.to_le_bytes());
        }
        if let Err(err) = stream.write_all(&frame).and_then(|()| stream.flush()) {
            self.close(&sent);
            return Err(err).context("sending handles to the successor");
        }
        Ok(())
    }

    /// Close handles put into the successor that it will never hear of.
    fn close(&self, sent: &[u64]) {
        use winapi::um::handleapi::DuplicateHandle;
        use winapi::um::winnt::DUPLICATE_CLOSE_SOURCE;
        for &value in sent {
            unsafe {
                DuplicateHandle(
                    self.0.as_raw_handle() as _,
                    value as usize as _,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    0,
                    0,
                    DUPLICATE_CLOSE_SOURCE,
                )
            };
        }
    }
}

/// The handles the predecessor put into this process for the message just
/// read; refused unless there are exactly `expected`.
#[cfg(windows)]
fn recv_handles(stream: &mut UnixStream, expected: usize) -> anyhow::Result<Vec<OwnedHandle>> {
    let mut count = [0u8; 4];
    stream
        .read_exact(&mut count)
        .context("reading a handle count")?;
    let count = u32::from_le_bytes(count) as usize;
    if count > 64 {
        bail!("the predecessor claims to have sent {count} handles");
    }
    // All of them owned before anything is judged, so none leaks.
    let mut handles = Vec::with_capacity(count);
    for _ in 0..count {
        let mut value = [0u8; 8];
        stream.read_exact(&mut value).context("reading a handle")?;
        let value = u64::from_le_bytes(value) as usize;
        handles.push(unsafe { OwnedHandle::from_raw_handle(value as RawHandle) });
    }
    if count != expected {
        bail!("expected {expected} handles, the predecessor sent {count}");
    }
    Ok(handles)
}

/// The pid of the process at the other end of an AF_UNIX stream.
#[cfg(windows)]
fn peer_pid(stream: &UnixStream) -> anyhow::Result<u32> {
    use std::os::windows::io::AsRawSocket;
    use winapi::um::winsock2::{WSAGetLastError, WSAIoctl, SOCKET};
    /// `_WSAIOR(IOC_VENDOR, 256)` from afunix.h.
    const SIO_AF_UNIX_GETPEERPID: u32 = 0x5800_0100;
    let mut pid: u32 = 0;
    let mut returned: u32 = 0;
    let rc = unsafe {
        WSAIoctl(
            stream.as_raw_socket() as SOCKET,
            SIO_AF_UNIX_GETPEERPID,
            std::ptr::null_mut(),
            0,
            &mut pid as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
            &mut returned,
            std::ptr::null_mut(),
            None,
        )
    };
    if rc != 0 || pid == 0 {
        bail!("asking for the successor's pid failed: WSA error {}", unsafe {
            WSAGetLastError()
        });
    }
    Ok(pid)
}

/// Who a process runs as, for comparing two: its user's SID and its
/// integrity level.
#[cfg(windows)]
fn token_identity(process: RawHandle) -> anyhow::Result<(Vec<u8>, u32)> {
    use winapi::um::processthreadsapi::OpenProcessToken;
    use winapi::um::securitybaseapi::{GetLengthSid, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation};
    use winapi::um::winnt::{
        TokenIntegrityLevel, TokenUser, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY, TOKEN_USER,
    };

    fn information(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS) -> anyhow::Result<Vec<u64>> {
        let mut len = 0u32;
        unsafe { GetTokenInformation(token.as_raw_handle() as _, class, std::ptr::null_mut(), 0, &mut len) };
        // u64s, for the alignment of the structures read from it.
        let mut buf = vec![0u64; (len as usize + 7) / 8];
        let ok = unsafe {
            GetTokenInformation(token.as_raw_handle() as _, class, buf.as_mut_ptr() as *mut _, len, &mut len)
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error()).context("reading a process token");
        }
        Ok(buf)
    }

    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process as _, TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error()).context("opening a process token");
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token as _) };

    let user = information(&token, TokenUser)?;
    let sid = unsafe { (*(user.as_ptr() as *const TOKEN_USER)).User.Sid };
    let sid_bytes =
        unsafe { std::slice::from_raw_parts(sid as *const u8, GetLengthSid(sid) as usize) }.to_vec();

    let label = information(&token, TokenIntegrityLevel)?;
    let level_sid = unsafe { (*(label.as_ptr() as *const TOKEN_MANDATORY_LABEL)).Label.Sid };
    let level = unsafe {
        let count = *GetSidSubAuthorityCount(level_sid);
        *GetSidSubAuthority(level_sid, u32::from(count.saturating_sub(1)))
    };
    Ok((sid_bytes, level))
}

const KITTY_PLAYBACK_TAG: &[u8] = b"KGPA\x01";
const KITTY_VIRTUAL_TAG: &[u8] = b"KGPA\x02";
const KITTY_NUMBERS_TAG: &[u8] = b"KGPA\x03";
const KITTY_PLACEMENTS_TAG: &[u8] = b"KGPA\x04";
const KITTY_RELATIVES_TAG: &[u8] = b"KGPA\x05";
const SHELL_COMMANDS_TAG: &[u8] = b"TTSH\x01";
const PANE_LABEL_TAG: &[u8] = b"TTLB\x01";

fn encode_snapshot(
    snapshot: &TerminalSnapshot,
    graphics: &wezterm_term::KittyGraphicsSnapshot,
    shell: &wezterm_term::ShellCommandsSnapshot,
    label: Option<&str>,
) -> anyhow::Result<Vec<u8>> {
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
    // Last, after the Kitty parts. A successor from before those never reads
    // past the prefix; one that knows them refuses this, and with it the
    // handoff, so the old server carries on.
    if shell.reported {
        bytes.extend_from_slice(SHELL_COMMANDS_TAG);
        shell.serialize(&mut varbincode::Serializer::new(&mut bytes))
            .context("encoding the shell's command count")?;
    }
    // The pane's label lives in the mux, not the terminal; it goes last,
    // on the same terms as the shell's count.
    if let Some(label) = label {
        bytes.extend_from_slice(PANE_LABEL_TAG);
        label.serialize(&mut varbincode::Serializer::new(&mut bytes))
            .context("encoding the pane's label")?;
    }
    zstd::encode_all(&bytes[..], 3).context("compressing a terminal snapshot")
}

fn decode_snapshot(
    bytes: &[u8],
) -> anyhow::Result<(
    TerminalSnapshot,
    Option<wezterm_term::KittyGraphicsSnapshot>,
    Option<wezterm_term::ShellCommandsSnapshot>,
    Option<String>,
)> {
    let bytes = zstd::decode_all(bytes).context("decompressing a terminal snapshot")?;
    let mut reader = &bytes[..];
    let snapshot = TerminalSnapshot::deserialize(&mut varbincode::Deserializer::new(&mut reader))
        .context("decoding a terminal snapshot")?;
    let graphics = if reader.is_empty()
        || reader.starts_with(SHELL_COMMANDS_TAG)
        || reader.starts_with(PANE_LABEL_TAG)
    {
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
        Some(wezterm_term::KittyGraphicsSnapshot { playback, virtual_images, image_numbers, placements, relatives })
    };
    let shell = if reader.starts_with(SHELL_COMMANDS_TAG) {
        reader = &reader[SHELL_COMMANDS_TAG.len()..];
        Some(wezterm_term::ShellCommandsSnapshot::deserialize(&mut varbincode::Deserializer::new(&mut reader))
            .context("decoding the shell's command count")?)
    } else { None };
    let label = if reader.starts_with(PANE_LABEL_TAG) {
        reader = &reader[PANE_LABEL_TAG.len()..];
        Some(String::deserialize(&mut varbincode::Deserializer::new(&mut reader))
            .context("decoding the pane's label")?)
    } else { None };
    anyhow::ensure!(reader.is_empty(), "trailing bytes after the terminal snapshot's extensions");
    Ok((snapshot, graphics, shell, label))
}

// ------------------------------------------------------- the old server

#[cfg(unix)]
static PID_FILE_FD: AtomicI32 = AtomicI32::new(-1);
#[cfg(unix)]
static REPORT_FD: AtomicI32 = AtomicI32::new(-1);
/// Raw handles; 0 is none.
#[cfg(windows)]
static PID_FILE_HANDLE: AtomicUsize = AtomicUsize::new(0);
#[cfg(windows)]
static REPORT_HANDLE: AtomicUsize = AtomicUsize::new(0);

/// The pid file this daemon holds locked, to pass on with everything else.
#[cfg(unix)]
pub fn remember_pid_file_fd(fd: RawFd) {
    PID_FILE_FD.store(fd, Ordering::SeqCst);
}

/// The pipe back to the shell that ran `--daemonize --takeover`.
#[cfg(unix)]
pub fn remember_report_fd(fd: RawFd) {
    REPORT_FD.store(fd, Ordering::SeqCst);
}

/// The pid file this daemon holds open, which is its lock on Windows, to
/// pass on with everything else.
#[cfg(windows)]
pub fn remember_pid_file_handle(handle: RawHandle) {
    PID_FILE_HANDLE.store(handle as usize, Ordering::SeqCst);
}

/// The pipe back to the `--daemonize --takeover` launcher.
#[cfg(windows)]
pub fn remember_report_handle(handle: RawHandle) {
    REPORT_HANDLE.store(handle as usize, Ordering::SeqCst);
}

/// Tell the shell that ran `--daemonize --takeover` how it went, once;
/// nothing happens when it was not asked. The line is `ok` or
/// `error: <why>`, and the pipe closes with it.
pub fn report_takeover(outcome: Result<(), &str>) {
    write_report(match outcome {
        Ok(()) => "ok\n".to_string(),
        Err(why) => format!("error: {}\n", why.replace('\n', " ")),
    });
}

/// `report_takeover` for this process failing with `err`, and the status
/// it exits with: a takeover that could not start at all reports
/// `refused: <why>` and exits `TAKEOVER_CANNOT_HAND_OVER`, as trying again
/// would be no use; anything else is an error, and 1.
pub fn report_takeover_failure(err: &anyhow::Error) -> i32 {
    let why = format!("{err:#}").replace('\n', " ");
    if err.downcast_ref::<CannotHandOver>().is_some() {
        write_report(format!("refused: {why}\n"));
        mux::session_server::TAKEOVER_CANNOT_HAND_OVER
    } else {
        write_report(format!("error: {why}\n"));
        1
    }
}

#[cfg(unix)]
fn write_report(line: String) {
    let fd = REPORT_FD.swap(-1, Ordering::SeqCst);
    if fd < 0 {
        return;
    }
    let mut pipe = unsafe { std::fs::File::from_raw_fd(fd) };
    pipe.write_all(line.as_bytes()).ok();
}

#[cfg(windows)]
fn write_report(line: String) {
    let handle = REPORT_HANDLE.swap(0, Ordering::SeqCst);
    if handle == 0 {
        return;
    }
    let mut pipe = unsafe { std::fs::File::from_raw_handle(handle as RawHandle) };
    pipe.write_all(line.as_bytes()).ok();
}

/// Why a takeover could not start at all: nothing listens for a successor
/// (a server from before the handoff), the running server refused, or it
/// speaks another handoff version.
#[derive(Debug)]
pub struct CannotHandOver(String);

impl std::fmt::Display for CannotHandOver {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CannotHandOver {}

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
                "handed every pane to the successor in {:?}; this server stops serving",
                started.elapsed()
            );
            // Its panes' processes keep their local network access only
            // while it runs (see hold).
            #[cfg(target_os = "macos")]
            crate::hold::hold_instead_of_exiting();
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
    panes: Vec<(HandoffPane, PaneObjects)>,
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
    #[cfg(windows)]
    let successor = Successor::of(stream)?;
    let unix_domains = config::configuration().unix_domains.len();
    if unix_domains > 1 {
        bail!(
            "this server listens for {unix_domains} unix domains and a takeover carries one; \
             stop and restart it instead"
        );
    }
    #[cfg(unix)]
    let listener_fd = crate::local::listener_fd_for(socket_path)
        .ok_or_else(|| anyhow!("no listening socket registered for {}", socket_path.display()))?;
    #[cfg(windows)]
    let _ = socket_path;
    #[cfg(unix)]
    let pid_file_fd = PID_FILE_FD.load(Ordering::SeqCst);
    #[cfg(unix)]
    let has_pid_file = pid_file_fd >= 0;
    #[cfg(windows)]
    let pid_file_handle = PID_FILE_HANDLE.load(Ordering::SeqCst);
    #[cfg(windows)]
    let has_pid_file = pid_file_handle != 0;

    let capture = block_on_main(move || capture(has_pid_file))?;
    log::info!(
        "handing {} panes, {} windows to the successor ({} left behind)",
        capture.panes.len(),
        capture.topology.len(),
        capture.hello.left_behind.len()
    );

    write_message(stream, &HandoffMessage::Hello(capture.hello.clone()))?;
    if has_pid_file {
        #[cfg(unix)]
        send_fd(stream, pid_file_fd)?;
        #[cfg(windows)]
        successor
            .send(stream, &[pid_file_handle as RawHandle])
            .context("sending the pid file")?;
    }
    expect(stream, "Ack")?;

    for (pane, objects) in &capture.panes {
        let pane_id = pane.pane_id;
        write_message(stream, &HandoffMessage::Pane(clone_pane(pane)))
            .with_context(|| format!("sending pane {pane_id}"))?;
        #[cfg(unix)]
        send_fd(stream, objects.as_raw_fd())
            .with_context(|| format!("sending pane {pane_id}'s pty"))?;
        #[cfg(windows)]
        {
            let handles: Vec<RawHandle> = objects.iter().map(|h| h.as_raw_handle()).collect();
            successor
                .send(stream, &handles)
                .with_context(|| format!("sending pane {pane_id}'s console"))?;
        }
        expect(stream, "Ack").with_context(|| format!("pane {pane_id}"))?;
    }

    write_message(stream, &HandoffMessage::Topology(clone_topology(&capture.topology)))?;
    expect(stream, "Ack").context("topology")?;

    // On Windows the successor binds the socket itself once this server
    // has exited; the message stays, for the shape of the exchange.
    write_message(stream, &HandoffMessage::Listener)?;
    #[cfg(unix)]
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
            Some(local) if pane.domain_id() == local_domain => travelling_parts(local)
                .with_context(|| format!("pane {pane_id} cannot be handed over"))?,
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
        let (snapshot, graphics, shell) = local.snapshot_terminal();
        paused.push(guard);
        panes.push((
            HandoffPane {
                pane_id,
                description: parts.description,
                pid: parts.pid,
                tty_name: parts.tty_name,
                snapshot: encode_snapshot(
                    &snapshot,
                    &graphics,
                    &shell,
                    mux.pane_label(pane_id).as_deref(),
                )?,
            },
            parts.objects,
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

/// A pane's share of the handoff besides its terminal.
struct Parts {
    pid: u32,
    tty_name: Option<String>,
    description: String,
    objects: PaneObjects,
}

/// None for a pane that stays behind by design (see the module comment).
#[cfg(unix)]
fn travelling_parts(local: &LocalPane) -> anyhow::Result<Option<Parts>> {
    Ok(local.handoff_parts().map(|parts| Parts {
        pid: parts.pid,
        tty_name: parts
            .tty_name
            .map(|name| name.to_string_lossy().into_owned()),
        description: parts.description,
        objects: parts.pty_fd,
    }))
}

/// None for a pane that stays behind, by design or, as on unix, because
/// its parts could not be taken; an error only when no pane here can
/// travel (the system's pseudo console), which refuses the whole takeover
/// and leaves this server serving.
#[cfg(windows)]
fn travelling_parts(local: &LocalPane) -> anyhow::Result<Option<Parts>> {
    let parts = match local.handoff_parts() {
        None => return Ok(None),
        Some(Ok(parts)) => parts,
        Some(Err(err)) if !portable_pty::win::conpty::handoff_supported() => return Err(err),
        Some(Err(err)) => {
            log::warn!("a pane stays behind, its console could not be taken: {err:#}");
            return Ok(None);
        }
    };
    let mut objects = parts.conpty.into_handles();
    objects.push(parts.process);
    Ok(Some(Parts {
        pid: parts.pid,
        tty_name: None,
        description: parts.description,
        objects,
    }))
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
    pub pid_file: Option<PidFile>,
}

/// Connect to the running server's handoff socket and learn who it is.
/// Called before the mux exists: the runtime server id and the id
/// counters come from here.
pub fn begin(socket_path: &Path) -> anyhow::Result<Takeover> {
    let path = handoff_socket_path(socket_path);
    let mut stream = UnixStream::connect(&path).with_context(|| {
        CannotHandOver(format!(
            "connecting to {}: is a server that can hand over running?",
            path.display()
        ))
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
        HandoffMessage::Refused(why) => {
            return Err(CannotHandOver(format!("the running server refused: {why}")).into())
        }
        other => bail!("expected Hello, got {other:?}"),
    };
    if hello.version != HANDOFF_VERSION {
        return Err(CannotHandOver(format!(
            "the running server speaks handoff version {}, this one speaks {HANDOFF_VERSION}",
            hello.version
        ))
        .into());
    }
    let pid_file = if hello.pid_file {
        #[cfg(unix)]
        let pid_file = recv_owned_fd(&stream)?;
        #[cfg(windows)]
        let pid_file = recv_handles(&mut stream, 1)
            .context("receiving the pid file")?
            .remove(0);
        Some(pid_file)
    } else {
        None
    };
    stream.set_read_timeout(Some(STEP_TIMEOUT))?;
    write_message(&mut stream, &HandoffMessage::Ack)?;
    Ok(Takeover {
        stream,
        hello,
        pid_file,
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
    objects: PaneObjects,
    snapshot: TerminalSnapshot,
    graphics: Option<wezterm_term::KittyGraphicsSnapshot>,
    shell: Option<wezterm_term::ShellCommandsSnapshot>,
    label: Option<String>,
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
    #[cfg(unix)]
    let mut listener: Option<OwnedFd> = None;
    let started = Instant::now();

    loop {
        match read_message(&mut stream)? {
            HandoffMessage::Pane(pane) => {
                let pane_id = pane.pane_id;
                #[cfg(unix)]
                let objects = recv_owned_fd(&stream)?;
                #[cfg(windows)]
                let objects = recv_handles(
                    &mut stream,
                    portable_pty::win::conpty::ConPtyHandoff::HANDLES + 1,
                )
                .with_context(|| format!("receiving pane {pane_id}'s console"))?;
                fail_at("pane")?;
                let (snapshot, graphics, shell, label) = decode_snapshot(&pane.snapshot)
                    .with_context(|| format!("pane {pane_id}'s snapshot"))?;
                received.push(Received { pane, objects, snapshot, graphics, shell, label });
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
                #[cfg(unix)]
                {
                    listener = Some(recv_owned_fd(&stream)?);
                }
                fail_at("listener")?;
                write_message(&mut stream, &HandoffMessage::Ack)?;
            }
            HandoffMessage::Commit => {
                fail_at("commit")?;
                #[cfg(unix)]
                let listener = listener.ok_or_else(|| anyhow!("Commit without a listener"))?;
                install(received, windows, local_domain)?;
                #[cfg(unix)]
                start_listener(listener, socket_path)?;
                #[cfg(windows)]
                let _ = socket_path;
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
    local_domain: DomainId,
) -> anyhow::Result<()> {
    let mut staged = Staged {
        panes: Vec::new(),
        committed: false,
    };
    let mut by_id: HashMap<PaneId, Arc<dyn Pane>> = HashMap::new();
    let mut labels = Vec::new();
    for item in received {
        let pane_id = item.pane.pane_id;
        let label = item.label.clone();
        let pane = adopt_pane(item, local_domain)
            .with_context(|| format!("adopting pane {pane_id}"))?;
        if let Some(label) = label {
            labels.push((pane.pane_id(), label));
        }
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
    // Labelled once the mux has the panes: it labels only its own.
    let mux = Mux::get();
    for (pane_id, label) in labels {
        mux.set_pane_label(pane_id, Some(&label));
    }
    Ok(())
}

/// Accept on the listening socket the predecessor handed over.
#[cfg(unix)]
fn start_listener(listener: OwnedFd, socket_path: &Path) -> anyhow::Result<()> {
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
#[cfg(unix)]
fn ensure_blocking(fd: &OwnedFd) -> anyhow::Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd)?;
    if flags.contains(rustix::fs::OFlags::NONBLOCK) {
        rustix::fs::fcntl_setfl(fd, flags.difference(rustix::fs::OFlags::NONBLOCK))?;
    }
    Ok(())
}

/// The pty and the process of a received pane.
#[cfg(unix)]
fn adopt_pty(
    pane: &HandoffPane,
    fd: OwnedFd,
    _size: TerminalSize,
) -> anyhow::Result<(Box<dyn portable_pty::MasterPty>, Box<dyn portable_pty::Child + Send + Sync>)> {
    ensure_blocking(&fd).context("making an adopted pty blocking")?;
    let master =
        portable_pty::unix::master_from_raw_fd(fd, pane.tty_name.clone().map(PathBuf::from))?;
    Ok((master, portable_pty::adopted_child(pane.pid)))
}

/// The pseudo console and the process of a received pane, rebuilt from the
/// handles the predecessor put here. `size` is the snapshot's: a pseudo
/// console cannot be asked its size.
#[cfg(windows)]
fn adopt_pty(
    _pane: &HandoffPane,
    mut handles: Vec<OwnedHandle>,
    size: TerminalSize,
) -> anyhow::Result<(Box<dyn portable_pty::MasterPty>, Box<dyn portable_pty::Child + Send + Sync>)> {
    let process = handles
        .pop()
        .ok_or_else(|| anyhow!("a pane arrived without its process"))?;
    let console = portable_pty::win::conpty::ConPtyHandoff::from_handles(handles)?;
    let master = portable_pty::win::conpty::master_from_handoff(
        console,
        portable_pty::PtySize {
            rows: size.rows as u16,
            cols: size.cols as u16,
            pixel_width: size.pixel_width as u16,
            pixel_height: size.pixel_height as u16,
        },
    )?;
    Ok((master, portable_pty::win::adopted_child(process)))
}

fn adopt_pane(item: Received, domain_id: DomainId) -> anyhow::Result<Arc<dyn Pane>> {
    let Received { pane, objects, snapshot, graphics, shell, label: _ } = item;
    let (master, child) = adopt_pty(&pane, objects, snapshot.size)?;
    match build_terminal(&*master, snapshot, graphics, shell) {
        Ok((terminal, pane_writer)) => {
            let local = LocalPane::new(
                pane.pane_id,
                terminal,
                child,
                master,
                pane_writer,
                domain_id,
                pane.description,
            );
            Ok(Arc::new(local))
        }
        Err(err) => {
            // Closing an adopted pseudo console waits for its host to exit,
            // which it does not while the predecessor holds it: leave it to
            // this process's exit, as `Staged` does with whole panes.
            #[cfg(windows)]
            std::mem::forget(master);
            Err(err)
        }
    }
}

/// The terminal of an adopted pane, restored from its snapshot, and the
/// pane's writer.
fn build_terminal(
    master: &dyn portable_pty::MasterPty,
    snapshot: TerminalSnapshot,
    mut graphics: Option<wezterm_term::KittyGraphicsSnapshot>,
    shell: Option<wezterm_term::ShellCommandsSnapshot>,
) -> anyhow::Result<(wezterm_term::Terminal, Box<dyn std::io::Write + Send>)> {
    let writer = master.take_writer()?;
    let snapshot_size = snapshot.size;
    let mut terminal = wezterm_term::Terminal::new(
        snapshot_size,
        Arc::new(config::TermConfig::new()),
        config::TERM_PROGRAM,
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
    // Without it a shell that reported its commands would read as one that
    // never did, until its next prompt.
    if let Some(shell) = shell {
        terminal.restore_shell_commands(shell);
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
    Ok((terminal, pane_writer))
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

/// How long the successor waits for the old server to let go before it
/// reports the takeover as done all the same (see `wait_for_predecessor_exit`).
pub const LET_GO_TIMEOUT: Duration = Duration::from_secs(5);

/// Blocks until the old server has closed its end -- it is gone, or on
/// macOS holds nothing any more (see `hold`) -- or for `limit` at most:
/// whether it did. Until then it may still accept on the socket.
pub fn wait_for_predecessor_exit(stream: &mut UnixStream, limit: Option<Duration>) -> bool {
    stream.set_read_timeout(limit).ok();
    let mut buf = [0u8; 64];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return false
            }
            Err(_) => return true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn an_adopted_nonblocking_descriptor_becomes_blocking() {
        let (end, _peer) = UnixStream::pair().unwrap();
        end.set_nonblocking(true).unwrap();
        let fd = OwnedFd::from(end);
        ensure_blocking(&fd).unwrap();
        let flags = rustix::fs::fcntl_getfl(&fd).unwrap();
        assert!(!flags.contains(rustix::fs::OFlags::NONBLOCK));
    }

    #[cfg(unix)]
    #[test]
    fn a_descriptor_waits_for_room_in_a_full_stream() {
        let (sender, mut receiver) = UnixStream::pair().unwrap();
        sender.set_write_timeout(Some(STEP_TIMEOUT)).unwrap();
        receiver.set_read_timeout(Some(STEP_TIMEOUT)).unwrap();
        // Full to the last byte, as a pane's message can leave it.
        sender.set_nonblocking(true).unwrap();
        let mut filled = 0;
        for chunk in [65536, 4096, 256, 16, 1] {
            let bytes = vec![0u8; chunk];
            loop {
                match (&sender).write(&bytes) {
                    Ok(len) => filled += len,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(err) => panic!("filling the stream: {}", err),
                }
            }
        }
        sender.set_nonblocking(false).unwrap();
        let (passed, mut other_end) = UnixStream::pair().unwrap();
        // What a single try meets on macOS, and the wait is for; elsewhere
        // the send waits for the reader by itself.
        #[cfg(target_os = "macos")]
        {
            // Briefly: should a later macOS wait instead, this fails at once
            // rather than after a whole step.
            sender.set_write_timeout(Some(Duration::from_millis(200))).unwrap();
            let refused = sender.as_raw_fd().send_fd(passed.as_raw_fd()).unwrap_err();
            assert_eq!(refused.raw_os_error(), Some(libc::EMSGSIZE));
            sender.set_write_timeout(Some(STEP_TIMEOUT)).unwrap();
        }
        let reader = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let mut bytes = vec![0u8; filled];
            receiver.read_exact(&mut bytes).unwrap();
            recv_owned_fd(&receiver).unwrap()
        });
        send_fd(&sender, passed.as_raw_fd()).unwrap();
        let mut received = UnixStream::from(reader.join().unwrap());
        received.write_all(b"ok").unwrap();
        let mut reply = [0u8; 2];
        other_end.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"ok");
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

    #[cfg(unix)]
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
        let shell = term.snapshot_shell_commands();
        let bytes = encode_snapshot(&snapshot, &graphics, &shell, None).unwrap();
        assert_eq!(decode_snapshot(&bytes).unwrap(), (snapshot, None, None, None));
        let snapshot = term.snapshot();
        graphics.playback.selections.push(wezterm_term::kitty_animation::KittyPlaybackEntry {
            image_id: 1,
            data_hash: [7; 32],
            animation: wezterm_term::kitty_animation::KittyAnimation::new([0, 40], 100),
        });
        let bytes = encode_snapshot(&snapshot, &graphics, &shell, None).unwrap();
        let decoded = decode_snapshot(&bytes).unwrap();
        assert_eq!(decoded, (term.snapshot(), Some(graphics.clone()), None, None));
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
            let bytes = encode_snapshot(&snapshot, &graphics, &shell, None).unwrap();
            assert_eq!(decode_snapshot(&bytes).unwrap(), (term.snapshot(), Some(graphics.clone()), None, None));
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
            let bytes = encode_snapshot(&snapshot, &graphics, &shell, None).unwrap();
            assert_eq!(decode_snapshot(&bytes).unwrap(), (term.snapshot(), Some(graphics.clone()), None, None));
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
        let bytes = encode_snapshot(&snapshot, &graphics, &shell, None).unwrap();
        assert_eq!(decode_snapshot(&bytes).unwrap(), (term.snapshot(), Some(graphics), None, None));
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
    fn the_shell_command_count_travels_after_the_snapshot() {
        let mut term = wezterm_term::Terminal::new(
            TerminalSize { rows: 4, cols: 10, pixel_width: 80, pixel_height: 64, dpi: 96 },
            Arc::new(config::TermConfig::new()), "ThinkTerm", "test", Box::new(Vec::new()));
        term.advance_bytes("\x1b]133;A;cl=m;aid=1\x07$ \x1b]133;B\x07true\r\n\x1b]133;C\x07");
        term.advance_bytes("\x1b]133;D;3;aid=1\x07\x1b]133;A;cl=m;aid=1\x07");
        let snapshot = term.snapshot();
        let mut graphics = term.snapshot_kitty_graphics();
        let shell = term.snapshot_shell_commands();
        assert!(shell.reported);
        assert_eq!(shell.last_exit, Some(3));
        // On its own and after a Kitty part: either way the predecessor's
        // decoder reads the same prefix.
        for kitty in [false, true] {
            if kitty {
                graphics.playback.selections.push(wezterm_term::kitty_animation::KittyPlaybackEntry {
                    image_id: 1,
                    data_hash: [7; 32],
                    animation: wezterm_term::kitty_animation::KittyAnimation::new([0, 40], 100),
                });
            }
            let bytes = encode_snapshot(&snapshot, &graphics, &shell, None).unwrap();
            assert_eq!(
                decode_snapshot(&bytes).unwrap(),
                (term.snapshot(), kitty.then(|| graphics.clone()), Some(shell.clone()), None)
            );
            let raw = zstd::decode_all(&bytes[..]).unwrap();
            let mut reader = &raw[..];
            let old = TerminalSnapshot::deserialize(&mut varbincode::Deserializer::new(&mut reader)).unwrap();
            assert_eq!(old, snapshot);
            assert!(reader.starts_with(if kitty { KITTY_PLAYBACK_TAG } else { SHELL_COMMANDS_TAG }));
            assert!(decode_snapshot(&zstd::encode_all(&raw[..raw.len() - 1], 3).unwrap()).is_err());
            let mut trailing = raw; trailing.push(0);
            assert!(decode_snapshot(&zstd::encode_all(&trailing[..], 3).unwrap()).is_err());
        }
    }

    #[test]
    fn a_pane_label_travels_last() {
        let mut term = wezterm_term::Terminal::new(
            TerminalSize { rows: 4, cols: 10, pixel_width: 80, pixel_height: 64, dpi: 96 },
            Arc::new(config::TermConfig::new()), "ThinkTerm", "test", Box::new(Vec::new()));
        let graphics = term.snapshot_kitty_graphics();
        // Alone, and after the shell's count.
        for reported in [false, true] {
            if reported {
                term.advance_bytes("\x1b]133;A;cl=m;aid=1\x07$ \x1b]133;B\x07true\r\n\x1b]133;C\x07");
                term.advance_bytes("\x1b]133;D;0;aid=1\x07");
            }
            let snapshot = term.snapshot();
            let shell = term.snapshot_shell_commands();
            assert_eq!(shell.reported, reported);
            let bytes = encode_snapshot(&snapshot, &graphics, &shell, Some("reviewer")).unwrap();
            assert_eq!(
                decode_snapshot(&bytes).unwrap(),
                (term.snapshot(), None, reported.then(|| shell.clone()), Some("reviewer".to_string()))
            );
            let raw = zstd::decode_all(&bytes[..]).unwrap();
            let mut reader = &raw[..];
            let old = TerminalSnapshot::deserialize(&mut varbincode::Deserializer::new(&mut reader)).unwrap();
            assert_eq!(old, snapshot);
            assert!(reader.starts_with(if reported { SHELL_COMMANDS_TAG } else { PANE_LABEL_TAG }));
            assert!(decode_snapshot(&zstd::encode_all(&raw[..raw.len() - 1], 3).unwrap()).is_err());
            let mut trailing = raw; trailing.push(0);
            assert!(decode_snapshot(&zstd::encode_all(&trailing[..], 3).unwrap()).is_err());
        }
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
            let shell = terminal.snapshot_shell_commands();
            let bytes = encode_snapshot(&snapshot, &graphics, &shell, None).unwrap();
            let (decoded, state, _, _) = decode_snapshot(&bytes).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn waiting_for_the_predecessor_tells_whether_it_let_go() {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        assert!(!wait_for_predecessor_exit(&mut ours, Some(Duration::from_millis(50))));
        drop(theirs);
        assert!(wait_for_predecessor_exit(&mut ours, Some(Duration::from_millis(50))));
    }

    #[test]
    fn the_handoff_socket_sits_next_to_the_socket() {
        assert_eq!(
            handoff_socket_path(Path::new("/run/user/1000/thinkterm/sock")),
            PathBuf::from("/run/user/1000/thinkterm/sock.handoff")
        );
    }
}
