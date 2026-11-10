use crate::domain::DomainId;
use crate::pane::{
    BatchRead, CachePolicy, CloseReason, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId,
    PaneSummary, Pattern, SearchResult, WithPaneLines,
};
use crate::renderable::*;
use crate::tmux::{TmuxDomain, TmuxDomainState};
use crate::{Domain, Mux, MuxNotification};
use anyhow::Error;
use async_trait::async_trait;
use config::keyassignment::ScrollbackEraseMode;
use config::{configuration, ExitBehavior, ExitBehaviorMessaging};
use fancy_regex::Regex;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
use procinfo::LocalProcessInfo;
use rangeset::RangeSet;
use smol::channel::{bounded, Receiver, TryRecvError};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::TryInto;
use std::io::{Result as IoResult, Write};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::escape::csi::{Sgr, CSI};
use termwiz::escape::{Action, DeviceControlMode};
use termwiz::input::KeyboardEncoding;
use termwiz::surface::{Line, SequenceNo};
use url::Url;
use wezterm_dynamic::Value;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Alert, AlertHandler, Clipboard, DownloadHandler, KeyCode, KeyModifiers, MouseEvent, Progress,
    SemanticZone, StableRowIndex, Terminal, TerminalConfiguration, TerminalSize,
};

const PROC_INFO_CACHE_TTL: Duration = Duration::from_millis(300);

fn connect_plain_text(
    term: &Terminal,
    options: thinkterm_reverse_access::protocol::ReadOptions,
) -> thinkterm_reverse_access::Result<String> {
    use thinkterm_reverse_access::{Error, MAX_READ_BYTES, MAX_READ_LINES};
    let screen = term.screen();
    let rows = screen.scrollback_rows();
    let top = rows.saturating_sub(screen.physical_rows);
    let row = |relative: i64| (top as i128 + relative as i128).max(0);
    let start = options.start.map(row).unwrap_or(top as i128);
    let end = options.end.map(row).unwrap_or(rows as i128);
    if start > end {
        return Err(Error::Protocol);
    }
    let start = start.min(rows as i128) as usize;
    let end = (end + 1).min(rows as i128) as usize;
    if end - start > MAX_READ_LINES {
        return Err(Error::TooLarge);
    }
    let mut text = String::with_capacity(MAX_READ_BYTES);
    let mut result = Ok(());
    let mut logical_start = 0;
    let mut keep = 0;
    let mut last_nonempty_end = 0;
    screen.with_phys_lines(start..end, |lines| {
        for (index, line) in lines.iter().enumerate() {
            for cell in line.visible_cells() {
                let value = cell.str();
                if value.len() >= MAX_READ_BYTES - text.len() {
                    result = Err(Error::TooLarge);
                    return;
                }
                text.push_str(value);
                if !value.trim().is_empty() {
                    keep = text.len();
                }
            }
            if !options.unwrap || !line.last_cell_was_wrapped() || index + 1 == lines.len() {
                if options.trim {
                    text.truncate(keep.max(logical_start));
                }
                if text.len() == MAX_READ_BYTES {
                    result = Err(Error::TooLarge);
                    return;
                }
                let nonempty = text.len() > logical_start;
                text.push('\n');
                if nonempty {
                    last_nonempty_end = text.len();
                }
                logical_start = text.len();
                keep = logical_start;
            }
        }
    });
    result?;
    if options.trim {
        text.truncate(last_nonempty_end);
    }
    Ok(text)
}

#[derive(Debug)]
enum ProcessState {
    Running {
        child_waiter: Receiver<IoResult<ExitStatus>>,
        pid: Option<u32>,
        signaller: Box<dyn ChildKiller + Sync>,
        // Whether we've explicitly killed the child
        killed: bool,
    },
    DeadPendingClose {
        killed: bool,
    },
    Dead,
}

struct CachedProcInfo {
    root: LocalProcessInfo,
    updated: Instant,
    foreground: LocalProcessInfo,
}

/// This is a bit horrible; it can take 700us to tcgetpgrp, so if we have
/// 10 tabs open and run the mouse over them, hovering them each in turn,
/// we can spend 7ms per evaluation of the tab bar state on fetching those
/// pids alone, which can easily lead to stuttering when moving the mouse
/// over all of the tabs.
///
/// This implements a cache holding that fg process and the often queried
/// cwd and process path that allows for stale reads to proceed quickly
/// while the writes can happen in a background thread.
#[cfg(unix)]
#[derive(Clone)]
struct CachedLeaderInfo {
    updated: Instant,
    fd: std::os::fd::RawFd,
    pid: u32,
    path: Option<std::path::PathBuf>,
    current_working_dir: Option<std::path::PathBuf>,
    updating: bool,
    /// Bumped on every refresh claim and on every FetchImmediate
    /// replacement. A background refresh may only write its results back
    /// while the claim it snapshotted is still current; anything else
    /// means fresher data landed in the meantime and the stale results
    /// must be discarded.
    claim: u64,
}

#[cfg(unix)]
impl CachedLeaderInfo {
    fn new(fd: Option<std::os::fd::RawFd>, claim: u64) -> Self {
        let mut me = Self {
            updated: Instant::now(),
            fd: fd.unwrap_or(-1),
            pid: 0,
            path: None,
            current_working_dir: None,
            updating: false,
            claim,
        };
        me.update();
        me
    }

    fn can_update(&self) -> bool {
        self.fd != -1 && !self.updating
    }

    /// The actual probe: pure syscalls against the fd, no locks, no shared
    /// state. tcgetpgrp reports errors as -1, which must not survive the
    /// cast to u32 or the process-info lookups chase pid 4294967295.
    fn probe(
        fd: std::os::fd::RawFd,
    ) -> (u32, Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
        let pgrp = unsafe { libc::tcgetpgrp(fd) };
        if pgrp > 0 {
            let pid = pgrp as u32;
            (
                pid,
                LocalProcessInfo::executable_path(pid),
                LocalProcessInfo::current_working_dir(pid),
            )
        } else {
            (0, None, None)
        }
    }

    fn update(&mut self) {
        let (pid, path, cwd) = Self::probe(self.fd);
        self.pid = pid;
        self.path = path;
        self.current_working_dir = cwd;
        self.updated = Instant::now();
        self.updating = false;
    }

    fn expired(&self) -> bool {
        self.updated.elapsed() > PROC_INFO_CACHE_TTL
    }
}

/// Refreshes stale [`CachedLeaderInfo`] entries on smol's blocking pool
/// instead of spawning a short-lived OS thread per pane per refresh (the
/// old scheme cost a thread creation up to every 300ms per pane). The pool
/// reuses warm threads and grows on demand, so one probe blocking forever
/// (a cwd on a dead network mount, a D-state process) delays only its own
/// pane instead of wedging every pane behind a single worker, and there is
/// no request queue to overflow or fail to start.
#[cfg(unix)]
mod leader_refresh {
    use super::CachedLeaderInfo;
    use parking_lot::Mutex;
    use std::sync::{Arc, Weak};

    type LeaderCell = Mutex<Option<CachedLeaderInfo>>;

    /// Queue a refresh of `leader`. The per-entry `updating` flag caps the
    /// outstanding work at one task per pane.
    ///
    /// Deliberately never takes the leader lock: the caller holds it.
    pub(super) fn request_refresh(leader: &Arc<LeaderCell>) {
        let weak = Arc::downgrade(leader);
        smol::unblock(move || refresh_one(&weak)).detach();
    }

    fn refresh_one(weak: &Weak<LeaderCell>) {
        // The upgrade proves the cache cell is alive, not the pane: the pty
        // (and its fd) can be gone by the time the probe runs, same as the
        // old per-refresh thread. The probe is read-only and its result
        // then lands in a cache nobody reads again, so that race is
        // harmless; the upgrade just stops queued work for dead panes.
        let Some(cell) = weak.upgrade() else { return };
        let Some((fd, claim)) = snapshot(&cell) else {
            return;
        };
        // The syscalls run without the lock so stale reads stay fast.
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| CachedLeaderInfo::probe(fd)));
        match result {
            Ok((pid, path, cwd)) => write_back(&cell, claim, pid, path, cwd),
            Err(err) => {
                // A panic must not strand the entry mid-claim (can_update()
                // would stay false for the pane's whole life), but the flag
                // may only be cleared while the claim is still ours: a
                // newer claimant owns it otherwise.
                if let Some(info) = cell.lock().as_mut() {
                    if info.claim == claim {
                        info.updating = false;
                    }
                }
                log::error!("leader refresh panicked: {err:?}");
            }
        }
    }

    fn snapshot(cell: &LeaderCell) -> Option<(std::os::fd::RawFd, u64)> {
        let guard = cell.lock();
        match guard.as_ref() {
            Some(info) if info.updating && info.fd != -1 => Some((info.fd, info.claim)),
            _ => None,
        }
    }

    fn write_back(
        cell: &LeaderCell,
        claim: u64,
        pid: u32,
        path: Option<std::path::PathBuf>,
        cwd: Option<std::path::PathBuf>,
    ) {
        let mut guard = cell.lock();
        if let Some(info) = guard.as_mut() {
            if info.claim == claim {
                info.pid = pid;
                info.path = path;
                info.current_working_dir = cwd;
                info.updated = std::time::Instant::now();
                info.updating = false;
            }
            // On mismatch the results are discarded and `updating` is left
            // alone: it belongs to whoever holds the newer claim now.
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::Instant;

        fn entry(fd: std::os::fd::RawFd, claim: u64) -> CachedLeaderInfo {
            CachedLeaderInfo {
                updated: Instant::now(),
                fd,
                pid: 0,
                path: None,
                current_working_dir: None,
                updating: true,
                claim,
            }
        }

        #[test]
        fn probe_reports_tcgetpgrp_errors_as_no_leader() {
            // tcgetpgrp(-1) fails with -1; the old `as u32` cast turned
            // that into pid 4294967295 and chased its process info.
            let (pid, path, cwd) = CachedLeaderInfo::probe(-1);
            assert_eq!(pid, 0);
            assert!(path.is_none());
            assert!(cwd.is_none());
        }

        #[test]
        fn refresh_writes_back_and_clears_the_claim() {
            let file = std::fs::File::open("/dev/null").unwrap();
            let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
            let cell = Arc::new(Mutex::new(Some(entry(fd, 7))));
            refresh_one(&Arc::downgrade(&cell));
            let guard = cell.lock();
            let info = guard.as_ref().unwrap();
            assert!(!info.updating, "refresh must release the claim");
            assert_eq!(info.pid, 0, "/dev/null has no foreground group");
            assert_eq!(info.claim, 7);
        }

        #[test]
        fn stale_write_back_is_discarded() {
            // Simulates a FetchImmediate (or newer claim) landing while the
            // worker was mid-probe: the claim moved on, so the older
            // results must not clobber the fresher entry.
            let cell = Arc::new(Mutex::new(Some(CachedLeaderInfo {
                pid: 42,
                path: Some(std::path::PathBuf::from("/fresh")),
                ..entry(-1, 8)
            })));
            write_back(&cell, 7, 0, Some(std::path::PathBuf::from("/stale")), None);
            let guard = cell.lock();
            let info = guard.as_ref().unwrap();
            assert_eq!(info.pid, 42);
            assert_eq!(info.path.as_deref(), Some(std::path::Path::new("/fresh")));
            assert!(
                info.updating,
                "the newer claim's refresh still owns the flag"
            );
        }

        #[test]
        fn matching_write_back_applies() {
            let cell = Arc::new(Mutex::new(Some(entry(-1, 9))));
            write_back(&cell, 9, 5, Some(std::path::PathBuf::from("/bin/sh")), None);
            let guard = cell.lock();
            let info = guard.as_ref().unwrap();
            assert_eq!(info.pid, 5);
            assert!(!info.updating);
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LocalPaneConnectionState {
    Connecting,
    Connected,
}

pub struct LocalPane {
    pane_id: PaneId,
    terminal: Mutex<Terminal>,
    process: Mutex<ProcessState>,
    pty: Arc<Mutex<Box<dyn MasterPty>>>,
    writer: Mutex<Box<dyn Write + Send>>,
    domain_id: DomainId,
    tmux_domain: Mutex<Option<Arc<TmuxDomainState>>>,
    proc_list: Mutex<Option<CachedProcInfo>>,
    #[cfg(unix)]
    leader: Arc<Mutex<Option<CachedLeaderInfo>>>,
    command_description: String,
    /// The summary as of the last time the terminal could be read: what
    /// `summary_without_waiting` hands out while the parser holds the
    /// terminal. Refreshed by the parser after every batch, and by every
    /// read that gets the lock.
    last_summary: Mutex<PaneSummary>,
    /// A size the pty already has but the terminal does not yet: `resize`
    /// found the parser holding the terminal, and the parser applies it
    /// around its next batch. Only ever the newest one.
    pending_resize: Mutex<Option<TerminalSize>>,
    /// The terminal's configuration, as `set_config` last set it, so that
    /// `get_config` -- read for every split and spawn that inherits it --
    /// never waits for the parser.
    config: Mutex<Arc<dyn TerminalConfiguration>>,
    /// A configuration the terminal has yet to take, as `pending_resize`.
    pending_config: Mutex<Option<Arc<dyn TerminalConfiguration>>>,
    /// A focus change the terminal has yet to hear of, as `pending_resize`:
    /// focusing a pane is what a person does right before typing into it,
    /// and the main thread must not wait on that pane's parser for it.
    pending_focus: Mutex<Option<bool>>,
    /// A look at the pane the terminal has yet to hear of, as
    /// `pending_focus`: it retires finished programs' OSC 7501 results.
    pending_seen: Mutex<bool>,
    /// Whether the pane's root was a shell without children, and when:
    /// finding out walks the process table, so the answer stands a moment.
    shell_idle: Mutex<Option<(Instant, bool)>>,
    /// The progress the terminal last reported, for `get_progress` while
    /// the parser holds the terminal.
    last_progress: Mutex<Progress>,
    /// Waiting for the batch of output being applied to end; see
    /// `Pane::read_after_batch`.
    batch_reads: Mutex<Vec<BatchRead>>,
    /// What the shell last reported about its commands, for `run_state`
    /// while the parser holds the terminal.
    last_shell_commands: Mutex<wezterm_term::ShellCommands>,
    /// How the program ended, once `is_dead` has seen it end.
    exit: Mutex<Option<thinkterm_proto::PaneExit>>,
}

#[async_trait(?Send)]
impl Pane for LocalPane {
    fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    fn get_metadata(&self) -> Value {
        #[allow(unused_mut)]
        let mut map: BTreeMap<Value, Value> = BTreeMap::new();

        #[cfg(unix)]
        if let Some(tio) = self.pty.lock().get_termios() {
            use nix::sys::termios::LocalFlags;
            // Detect whether we might be in password input mode.
            // If local echo is disabled and canonical input mode
            // is enabled, then we assume that we're in some kind
            // of password-entry mode.
            let pw_input = !tio.local_flags.contains(LocalFlags::ECHO)
                && tio.local_flags.contains(LocalFlags::ICANON);
            map.insert(
                Value::String("password_input".to_string()),
                Value::Bool(pw_input),
            );
        }

        Value::Object(map.into())
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        let tmux = self.tmux_domain.lock().is_some();
        PaneSummary::cursor_position_of(&mut self.terminal.lock(), tmux)
    }

    fn get_keyboard_encoding(&self) -> KeyboardEncoding {
        if self.tmux_domain.lock().is_some() {
            KeyboardEncoding::Xterm
        } else {
            self.terminal.lock().get_keyboard_encoding()
        }
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.terminal.lock().current_seqno()
    }

    fn render_state_is_contended(&self) -> bool {
        // The parser thread holds this for the whole of a batch of output;
        // a probe that fails is the batch still being applied.
        self.terminal.try_lock().is_none()
    }

    fn read_after_batch(&self, read: BatchRead) -> Result<(), BatchRead> {
        self.batch_reads.lock().push(read);
        Ok(())
    }

    fn hold_render_state_for_the_test(&self, hold: Duration) {
        let _held = self.terminal.lock();
        std::thread::sleep(hold);
    }

    fn summary_without_waiting(&self) -> PaneSummary {
        // The note before the snapshot: read the other way round, a
        // parser applying the note in between would leave a snapshot
        // from before the resize with no note to say so.
        let resize_pending = *self.pending_resize.lock();
        let mut summary = match self.terminal.try_lock() {
            Some(mut term) => self.store_summary(&mut term),
            None => self.last_summary.lock().clone(),
        };
        // Noted alongside a coherent snapshot, never patched into it:
        // that gave a screen no terminal ever showed, with a scrollback
        // offset from before the reflow.
        summary.resize_pending = resize_pending;
        summary
    }

    fn listing_summary(&self) -> PaneSummary {
        let mut summary = self.summary_without_waiting();
        self.spice_summary(&mut summary);
        summary
    }

    fn title_without_waiting(&self) -> String {
        let mut title = match self.terminal.try_lock() {
            Some(term) => term.get_title().to_string(),
            None => self.last_summary.lock().title.clone(),
        };
        self.spice_title(&mut title);
        title
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        terminal_get_dirty_lines(&mut self.terminal.lock(), lines, seqno)
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        terminal_for_each_logical_line_in_stable_range_mut(
            &mut self.terminal.lock(),
            lines,
            for_line,
        );
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        terminal_with_lines_mut(&mut self.terminal.lock(), lines, with_lines)
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        crate::pane::impl_get_lines_via_with_lines(self, lines)
    }

    fn get_screen_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        terminal_get_screen_lines(&mut self.terminal.lock(), lines)
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        crate::pane::impl_get_logical_lines_via_get_lines(self, lines)
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        terminal_get_dimensions(&mut self.terminal.lock())
    }

    fn copy_user_vars(&self) -> HashMap<String, String> {
        self.terminal.lock().user_vars().clone()
    }

    fn exit_behavior(&self) -> Option<ExitBehavior> {
        // If we are ssh, and we've not yet fully connected,
        // then override exit_behavior so that we can show
        // connection issues
        let mut pty = self.pty.lock();
        let is_ssh_connecting = pty
            .downcast_mut::<crate::ssh::WrappedSshPty>()
            .map(|s| s.is_connecting())
            .unwrap_or(false);
        let is_failed_spawn = pty.is::<crate::domain::FailedSpawnPty>();

        if is_ssh_connecting || is_failed_spawn {
            Some(ExitBehavior::CloseOnCleanExit)
        } else {
            None
        }
    }

    fn kill(&self) {
        let mut proc = self.process.lock();
        log::debug!(
            "killing process in pane {}, state is {:?}",
            self.pane_id,
            proc
        );
        match &mut *proc {
            ProcessState::Running {
                signaller, killed, ..
            } => {
                let _ = signaller.kill();
                *killed = true;
            }
            ProcessState::DeadPendingClose { killed } => {
                *killed = true;
            }
            _ => {}
        }
    }

    fn idle_image_tick(&self, ticks_until_release: u32) -> usize {
        self.terminal.lock().idle_image_tick(ticks_until_release)
    }

    fn image_stats(&self) -> (usize, usize, usize) {
        self.terminal.lock().kitty_image_stats()
    }

    fn kitty_frame_selections(&self, known: Option<u64>) -> Option<(u64, u64, Vec<wezterm_term::KittyFrameSelection>)> {
        self.terminal.lock().kitty_frame_selections(known).map(|(revision, selections)| (revision, 0, selections))
    }

    fn get_kitty_image(&self, request: thinkterm_proto::image::GetKittyImage) -> crate::pane::KittyImageFuture {
        // A program streaming frames replaces its picture before a fetch for
        // the previous one lands, which left the row blank, so a named image
        // answers with its newer picture, sent whole. Id 0 holds only the
        // newest anonymous image; any older one would be the wrong picture.
        let data = self.terminal.lock().kitty_image(request.image_id)
            .filter(|data| request.image_id != 0 || data.hash() == request.data_hash);
        Box::pin(async move {
            anyhow::ensure!(request.image_epoch.is_none_or(|epoch| epoch == 0), "Kitty image epoch changed");
            let (data_generation, data, frames_from) = match data {
                Some(data) => {
                    let have_frames = if data.hash() == request.data_hash { request.have_frames } else { 0 };
                    let (generation, payload, from) = thinkterm_proto::image::image_reply(&data, have_frames);
                    (generation, Some(payload), from)
                }
                None => (0, None, 0),
            };
            Ok(thinkterm_proto::image::GetImageCellResponse { pane_id: request.pane_id, data, data_generation, frames_from })
        })
    }

    fn read_remote_file(
        &self,
        request: thinkterm_proto::file::ReadRemoteFile,
    ) -> crate::pane::ReadRemoteFileFuture {
        // Chunks are bounded by the caller, but never trust the wire: a
        // single request may not ask the server for an unbounded buffer.
        const MAX_CHUNK: u32 = 8 * 1024 * 1024;
        let length = request.length.min(MAX_CHUNK);
        let pane_id = request.pane_id;
        let path = request.path;
        let offset = request.offset;
        Box::pin(async move {
            smol::unblock(move || {
                use std::io::{Read, Seek, SeekFrom};
                let mut file =
                    std::fs::File::open(&path).map_err(|err| anyhow::anyhow!("opening {path}: {err}"))?;
                let metadata = file
                    .metadata()
                    .map_err(|err| anyhow::anyhow!("reading {path} metadata: {err}"))?;
                anyhow::ensure!(metadata.is_file(), "{path} is not a regular file");
                let total = metadata.len();
                anyhow::ensure!(offset <= total, "offset {offset} is past the end of {path}");
                file.seek(SeekFrom::Start(offset))
                    .map_err(|err| anyhow::anyhow!("seeking {path}: {err}"))?;
                let mut data = vec![0u8; length as usize];
                let n = file
                    .read(&mut data)
                    .map_err(|err| anyhow::anyhow!("reading {path}: {err}"))?;
                data.truncate(n);
                Ok(thinkterm_proto::file::ReadRemoteFileResponse {
                    pane_id,
                    data,
                    eof: offset + n as u64 >= total,
                })
            })
            .await
        })
    }

    fn is_dead(&self) -> bool {
        let mut proc = self.process.lock();

        const EXIT_BEHAVIOR: &str = "This message is shown because \
            \x1b]8;;https://wezterm.org/\
            config/lua/config/exit_behavior.html\
            \x1b\\exit_behavior\x1b]8;;\x1b\\";

        fn clean_exit_message(cmd: &str) -> String {
            if cmd == "RemoteSshDomain" {
                "👍 SSH connection closed.".to_string()
            } else {
                format!("👍 Process {cmd} completed.")
            }
        }

        fn dirty_exit_message(cmd: &str) -> String {
            if cmd == "RemoteSshDomain" {
                "⚠️  Remote SSH process didn't exit cleanly.".to_string()
            } else {
                format!("⚠️  Process {cmd} didn't exit cleanly")
            }
        }

        let mut terse = String::new();
        let mut brief = String::new();
        let mut trailer = String::new();
        let cmd = &self.command_description;

        match &mut *proc {
            ProcessState::Running {
                child_waiter,
                killed,
                ..
            } => {
                let status = match child_waiter.try_recv() {
                    Ok(Ok(s)) => Some(s),
                    Err(TryRecvError::Empty) => None,
                    _ => Some(ExitStatus::with_exit_code(1)),
                };

                if let Some(status) = status {
                    *self.exit.lock() = Some(pane_exit(&status));
                    let success = exited_cleanly(&status, &configuration().clean_exit_codes);

                    match (
                        self.exit_behavior()
                            .unwrap_or_else(|| configuration().exit_behavior),
                        success,
                        killed,
                    ) {
                        (ExitBehavior::Close, _, _) => *proc = ProcessState::Dead,
                        (ExitBehavior::CloseOnCleanExit, false, _) => {
                            brief = dirty_exit_message(cmd);
                            terse = format!("{status}.");
                            trailer = format!("{EXIT_BEHAVIOR}=\"CloseOnCleanExit\"");

                            *proc = ProcessState::DeadPendingClose { killed: false }
                        }
                        (ExitBehavior::CloseOnCleanExit, ..) => *proc = ProcessState::Dead,
                        (ExitBehavior::Hold, success, false) => {
                            trailer = format!("{EXIT_BEHAVIOR}=\"Hold\"");

                            if success {
                                brief = clean_exit_message(cmd);
                                terse = "done".to_string();
                            } else {
                                brief = dirty_exit_message(cmd);
                                terse = format!("{status}");
                            }
                            *proc = ProcessState::DeadPendingClose { killed: false }
                        }
                        (ExitBehavior::Hold, _, true) => *proc = ProcessState::Dead,
                    }
                    log::debug!("child terminated, new state is {:?}", proc);
                }
            }
            ProcessState::DeadPendingClose { killed } => {
                if *killed {
                    *proc = ProcessState::Dead;
                    log::debug!("child state -> {:?}", proc);
                }
            }
            ProcessState::Dead => {}
        }

        let mut notify = None;
        if !terse.is_empty() {
            match configuration().exit_behavior_messaging {
                ExitBehaviorMessaging::Verbose => {
                    if terse == "done" {
                        notify = Some(format!("\r\n{brief}\r\n{trailer}"));
                    } else {
                        notify = Some(format!("\r\n{brief}\r\n{terse}\r\n{trailer}"));
                    }
                }
                ExitBehaviorMessaging::Brief => {
                    if terse == "done" {
                        notify = Some(format!("\r\n{brief}"));
                    } else {
                        notify = Some(format!("\r\n{brief}\r\n{terse}"));
                    }
                }
                ExitBehaviorMessaging::Terse => {
                    notify = Some(format!("\r\n[{terse}]"));
                }
                ExitBehaviorMessaging::None => {}
            }
        }

        if let Some(notify) = notify {
            emit_output_for_pane(self.pane_id, &notify);
        }

        match &*proc {
            ProcessState::Running { .. } => false,
            ProcessState::DeadPendingClose { .. } => false,
            ProcessState::Dead => true,
        }
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.terminal.lock().set_clipboard(clipboard);
    }

    fn set_download_handler(&self, handler: &Arc<dyn DownloadHandler>) {
        self.terminal.lock().set_download_handler(handler);
    }

    fn set_config(&self, config: Arc<dyn TerminalConfiguration>) {
        *self.config.lock() = Arc::clone(&config);
        // As a resize: now if the parser is not mid-batch, otherwise from
        // the parser around its next batch, which is what it would first
        // apply to anyway.
        match self.terminal.try_lock() {
            Some(mut term) => {
                *self.pending_config.lock() = None;
                term.set_config(config);
            }
            None => {
                *self.pending_config.lock() = Some(config);
                if let Some(mut term) = self.terminal.try_lock() {
                    self.apply_pending_config(&mut term);
                }
            }
        }
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        Some(Arc::clone(&self.config.lock()))
    }

    fn perform_actions(&self, mut actions: Vec<termwiz::escape::Action>) {
        self.perform_actions_in_place(&mut actions, true);
    }

    fn perform_actions_in_place(
        &self,
        actions: &mut Vec<termwiz::escape::Action>,
        may_hold_external_kitty: bool,
    ) {
        if may_hold_external_kitty && crate::has_external_kitty_image_data_source(actions) {
            let kitty_graphics_enabled =
                { self.terminal.lock().get_config().enable_kitty_graphics() };
            if kitty_graphics_enabled {
                crate::materialize_kitty_image_data_sources(actions);
            }
        }
        let mut term = self.terminal.lock();
        self.apply_pending_config(&mut term);
        self.apply_pending_resize(&mut term);
        self.apply_pending_focus(&mut term);
        term.perform_actions_in_place(actions);
        self.apply_pending_and_store(&mut term);
        drop(term);
        // This thread is the one that would take the terminal next, so
        // these find it free without having to be lucky.
        let reads = std::mem::take(&mut *self.batch_reads.lock());
        for read in reads {
            read(self);
        }
        // A resize that arrived after that last look, while the terminal
        // was still held here or read just now, found both of its own
        // probes failing and left its note; nothing else would read it
        // until the next batch, which a quiet program may never write.
        // Looked at again with the terminal free, for as long as notes
        // keep arriving: one left after the last look is applied by the
        // resize's own second probe, which finds the terminal free.
        while self.anything_pending() {
            let mut term = self.terminal.lock();
            self.apply_pending_and_store(&mut term);
        }
    }

    fn mouse_event(&self, event: MouseEvent) -> Result<(), Error> {
        Mux::get().record_input_for_current_identity();
        self.terminal.lock().mouse_event(event)
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> Result<(), Error> {
        Mux::get().record_input_for_current_identity();
        if self.tmux_domain.lock().is_some() {
            log::trace!("key: {:?}", key);
            if key == KeyCode::Char('q') {
                self.terminal.lock().send_paste("detach\n")?;
            }
            return Ok(());
        } else {
            self.terminal.lock().key_down(key, mods)
        }
    }

    fn key_up(&self, key: KeyCode, mods: KeyModifiers) -> Result<(), Error> {
        Mux::get().record_input_for_current_identity();
        self.terminal.lock().key_up(key, mods)
    }

    fn resize(&self, size: TerminalSize) -> Result<(), Error> {
        self.pty.lock().resize(PtySize {
            rows: size.rows.try_into()?,
            cols: size.cols.try_into()?,
            pixel_width: size.pixel_width.try_into()?,
            pixel_height: size.pixel_height.try_into()?,
        })?;
        // The pty has the size, so the program has its SIGWINCH. The
        // terminal gets it now if its parser is not in the middle of a
        // batch, and from the parser otherwise: a tab resize used to wait
        // here for every pane's parser in turn, on the thread that serves
        // every other pane and client.
        match self.terminal.try_lock() {
            Some(mut term) => {
                *self.pending_resize.lock() = None;
                term.resize(size);
                self.store_summary(&mut term);
            }
            None => {
                *self.pending_resize.lock() = Some(size);
                // The parser may have let go between the probe and the
                // note; it applies a note left during its batch, and this
                // applies one left after it.
                if let Some(mut term) = self.terminal.try_lock() {
                    self.apply_pending_resize(&mut term);
                    self.store_summary(&mut term);
                }
            }
        }
        Ok(())
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        Mux::get().record_input_for_current_identity();
        MutexGuard::map(self.writer.lock(), |writer| {
            let w: &mut dyn std::io::Write = writer;
            w
        })
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(Some(self.pty.lock().try_clone_reader()?))
    }

    #[cfg(unix)]
    fn pty_raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
        self.pty.lock().as_raw_fd()
    }

    fn send_paste(&self, text: &str) -> Result<(), Error> {
        Mux::get().record_input_for_current_identity();
        if self.tmux_domain.lock().is_some() {
            Ok(())
        } else {
            self.terminal.lock().send_paste(text)
        }
    }

    fn bracketed_paste(&self) -> Option<bool> {
        Some(self.terminal.lock().bracketed_paste_enabled())
    }

    fn get_title(&self) -> String {
        let mut title = self.terminal.lock().get_title().to_string();
        self.spice_title(&mut title);
        title
    }

    fn get_progress(&self) -> Progress {
        // Read for every session snapshot, on the main thread.
        match self.terminal.try_lock() {
            Some(term) => {
                let progress = term.get_progress();
                *self.last_progress.lock() = progress.clone();
                progress
            }
            None => self.last_progress.lock().clone(),
        }
    }

    fn agent_osc_evidence(&self) -> wezterm_term::AgentOscEvidence {
        self.terminal.lock().agent_osc_evidence()
    }

    fn clear_agent_osc_evidence(&self) {
        self.terminal.lock().clear_agent_osc_state();
    }

    fn program_status(&self) -> wezterm_term::program_status::ProgramStatusSnapshot {
        let exited = !matches!(&*self.process.lock(), ProcessState::Running { .. });
        let mut term = self.terminal.lock();
        if exited {
            // Held panes remain visible after their child exits. Finalize
            // each read, including any last reports parsed after the exit.
            term.end_program_status(u64::MAX);
        }
        term.program_status(wezterm_term::program_status::ProgramStatusRecords::MAX_RECORDS)
    }

    fn end_program_status(&self, through: u64) {
        self.terminal.lock().end_program_status(through);
    }

    fn program_status_seen(&self) {
        // Never waits on the parser, as `focus_changed` does not, and probes
        // again after leaving the mark: the parser may have made its last
        // check for pending work in between.
        match self.terminal.try_lock() {
            Some(mut term) => self.retire_seen_results(&mut term),
            None => {
                *self.pending_seen.lock() = true;
                if let Some(mut term) = self.terminal.try_lock() {
                    self.apply_pending_focus(&mut term);
                }
            }
        }
    }

    fn foreground_process_group(&self) -> Option<u32> {
        #[cfg(unix)]
        {
            // Only ever positive: the pty answers `None` for anything else.
            self.pty.lock().process_group_leader().map(|pid| pid as u32)
        }
        #[cfg(not(unix))]
        None
    }

    fn shell_is_foreground(&self) -> Option<bool> {
        #[cfg(unix)]
        {
            let leader = self.pty.lock().process_group_leader()?;
            let root = self.root_process_id()?;
            if leader as u32 != root {
                return Some(false);
            }
            // The pane's first process holds the foreground: a shell at its
            // prompt -- or the program the pane was opened on, or a shell
            // running a script or a `-c` command, which without job control
            // keeps its child in its own group. A childless shell may still
            // be executing a builtin: callers must also check who reported.
            const SHELL_IDLE_TTL: Duration = Duration::from_secs(2);
            let now = Instant::now();
            if let Some((at, idle)) = *self.shell_idle.lock() {
                if now.duration_since(at) < SHELL_IDLE_TTL {
                    return Some(idle);
                }
            }
            let path = procinfo::LocalProcessInfo::executable_path(root)?;
            let name = path.file_name()?.to_string_lossy();
            let shell = matches!(
                name.as_ref(),
                "sh" | "bash" | "zsh" | "fish" | "dash" | "ash" | "busybox" | "ksh" | "mksh"
                    | "tcsh" | "csh" | "nu" | "elvish" | "xonsh" | "pwsh"
            );
            let idle = shell
                && procinfo::LocalProcessInfo::with_root_pid(root)
                    .is_some_and(|info| info.children.is_empty());
            *self.shell_idle.lock() = Some((now, idle));
            Some(idle)
        }
        #[cfg(not(unix))]
        None
    }

    fn palette(&self) -> ColorPalette {
        self.terminal.lock().palette()
    }

    fn palette_override(&self) -> Option<ColorPalette> {
        self.terminal.lock().palette_override()
    }

    fn domain_id(&self) -> DomainId {
        self.domain_id
    }

    fn erase_scrollback(&self, erase_mode: ScrollbackEraseMode) {
        let mut term = self.terminal.lock();
        match erase_mode {
            ScrollbackEraseMode::ScrollbackOnly => term.erase_scrollback(),
            ScrollbackEraseMode::ScrollbackAndViewport => term.erase_scrollback_and_viewport(),
        }
        self.store_summary(&mut term);
    }

    fn focus_changed(&self, focused: bool) {
        match self.terminal.try_lock() {
            Some(mut term) => {
                *self.pending_focus.lock() = None;
                term.focus_changed(focused);
            }
            None => {
                *self.pending_focus.lock() = Some(focused);
                if let Some(mut term) = self.terminal.try_lock() {
                    self.apply_pending_focus(&mut term);
                }
            }
        }
    }

    fn has_unseen_output(&self) -> bool {
        self.terminal.lock().has_unseen_output()
    }

    fn is_mouse_grabbed(&self) -> bool {
        if self.tmux_domain.lock().is_some() {
            false
        } else {
            self.terminal.lock().is_mouse_grabbed()
        }
    }

    fn is_alt_screen_active(&self) -> bool {
        let tmux = self.tmux_domain.lock().is_some();
        PaneSummary::alt_screen_of(&self.terminal.lock(), tmux)
    }

    fn get_current_working_dir(&self, policy: CachePolicy) -> Option<Url> {
        let reported = self.terminal.lock().get_current_dir().cloned();
        self.working_dir_or_divined(reported, policy)
    }

    fn working_dir_without_waiting(&self, policy: CachePolicy) -> Option<Url> {
        // The reported directory from the terminal if it can be read
        // now, from the stored copy otherwise; the divination keeps the
        // caller's policy, since a spawn right after a `cd` in a shell
        // that reports nothing wants the process looked at now.
        let reported = self.summary_without_waiting().working_dir;
        self.working_dir_or_divined(reported, policy)
    }

    fn tty_name(&self) -> Option<String> {
        #[cfg(unix)]
        {
            let name = self.pty.lock().tty_name()?;
            Some(name.to_string_lossy().into_owned())
        }

        #[cfg(windows)]
        {
            None
        }
    }

    fn get_foreground_process_info(&self, policy: CachePolicy) -> Option<LocalProcessInfo> {
        #[cfg(unix)]
        if let Some(pid) = self.pty.lock().process_group_leader() {
            return LocalProcessInfo::with_root_pid(pid as u32);
        }

        self.divine_foreground_process(policy)
    }

    fn get_foreground_process_name(&self, policy: CachePolicy) -> Option<String> {
        #[cfg(unix)]
        {
            let leader = self.get_leader(policy);
            if let Some(path) = &leader.path {
                return Some(path.to_string_lossy().to_string());
            }
            return None;
        }

        #[cfg(windows)]
        if let Some(fg) = self.divine_foreground_process(policy) {
            return Some(fg.executable.to_string_lossy().to_string());
        }

        #[allow(unreachable_code)]
        None
    }

    fn get_foreground_process_argv(&self, policy: CachePolicy) -> Option<Vec<String>> {
        #[cfg(unix)]
        {
            let leader = self.get_leader(policy);
            if leader.pid > 0 {
                return LocalProcessInfo::argv_for_pid(leader.pid);
            }
            return None;
        }

        #[cfg(windows)]
        if let Some(fg) = self.divine_foreground_process(policy) {
            if !fg.argv.is_empty() {
                return Some(fg.argv);
            }
        }

        #[allow(unreachable_code)]
        None
    }

    fn root_process_id(&self) -> Option<u32> {
        match &*self.process.lock() {
            ProcessState::Running { pid, .. } => *pid,
            _ => None,
        }
    }

    fn can_close_without_prompting(&self, _reason: CloseReason) -> bool {
        if let Some(info) = self.divine_process_list(CachePolicy::FetchImmediate) {
            log::trace!(
                "can_close_without_prompting? procs in pane {:#?}",
                info.root
            );

            let hook_result = config::run_immediate_with_lua_config(|lua| {
                let lua = match lua {
                    Some(lua) => lua,
                    None => return Ok(None),
                };
                let v = config::lua::emit_sync_callback(
                    &*lua,
                    ("mux-is-process-stateful".to_string(), (info.root.clone())),
                )?;
                match v {
                    mlua::Value::Nil => Ok(None),
                    mlua::Value::Boolean(v) => Ok(Some(v)),
                    _ => Ok(None),
                }
            });

            fn default_stateful_check(proc_list: &LocalProcessInfo) -> bool {
                // Fig uses `figterm` a pseudo terminal for a lot of functionality, it runs between
                // the shell and terminal. Unfortunately it is typically named `<shell> (figterm)`,
                // which prevents the statuful check from passing. This strips the suffix from the
                // process name to allow the check to pass.
                let names = proc_list
                    .flatten_to_exe_names()
                    .into_iter()
                    .map(|s| match s.strip_suffix(" (figterm)") {
                        Some(s) => s.into(),
                        None => s,
                    })
                    .collect::<HashSet<_>>();

                let skip = configuration()
                    .skip_close_confirmation_for_processes_named
                    .iter()
                    .cloned()
                    .collect::<HashSet<_>>();

                if !names.is_subset(&skip) {
                    // There are other processes running than are listed,
                    // so we consider this to be stateful
                    return true;
                }
                false
            }

            let is_stateful = match hook_result {
                Ok(None) => default_stateful_check(&info.root),
                Ok(Some(s)) => s,
                Err(err) => {
                    log::error!(
                        "Error while running mux-is-process-stateful \
                         hook: {:#}, falling back to default behavior",
                        err
                    );
                    default_stateful_check(&info.root)
                }
            };

            !is_stateful
        } else {
            #[cfg(unix)]
            {
                // If the process is dead but exit_behavior is holding the
                // window, we don't need to prompt to confirm closing.
                // That is detectable as no longer having a process group leader.
                if self.pty.lock().process_group_leader().is_none() {
                    return true;
                }
            }

            false
        }
    }

    fn get_semantic_zones(&self) -> anyhow::Result<Vec<SemanticZone>> {
        let mut term = self.terminal.lock();
        term.get_semantic_zones()
    }

    async fn run_state(&self) -> anyhow::Result<thinkterm_proto::PaneRunState> {
        use thinkterm_proto::{PaneRunState, ProgramState, ShellCommands};
        // Asked over and over by a waiting `cli wait`, on the main thread:
        // never wait for the parser.
        let shell = match self.terminal.try_lock() {
            Some(term) => term.shell_commands(),
            None => *self.last_shell_commands.lock(),
        };
        let program = match &*self.process.lock() {
            ProcessState::Running { .. } => ProgramState::Running,
            ProcessState::DeadPendingClose { .. } | ProcessState::Dead => {
                match self.exit.lock().clone() {
                    Some(exit) => ProgramState::Exited(exit),
                    None => ProgramState::Unknown,
                }
            }
        };
        Ok(PaneRunState {
            program,
            shell: ShellCommands {
                reported: shell.reported,
                finished: shell.finished,
                running: shell.running,
                last_exit: shell.last_exit,
            },
        })
    }

    async fn search(
        &self,
        pattern: Pattern,
        range: Range<StableRowIndex>,
        limit: Option<u32>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let term = self.terminal.lock();
        let screen = term.screen();

        enum CompiledPattern {
            CaseSensitiveString(String),
            CaseInSensitiveString(String),
            Regex(Regex),
        }

        let pattern = match pattern {
            Pattern::CaseSensitiveString(s) => CompiledPattern::CaseSensitiveString(s),
            Pattern::CaseInSensitiveString(s) => {
                // normalize the case so we match everything lowercase
                CompiledPattern::CaseInSensitiveString(s.to_lowercase())
            }
            Pattern::Regex(r) => CompiledPattern::Regex(Regex::new(&r)?),
        };

        let mut results = vec![];
        let mut uniq_matches: HashMap<String, usize> = HashMap::new();

        screen.for_each_logical_line_in_stable_range(range, |sr, lines| {
            if let Some(limit) = limit {
                if results.len() == limit as usize {
                    // We've reach the limit, stop iteration.
                    return false;
                }
            }

            if lines.is_empty() {
                // Nothing to do on this iteration, carry on with the next.
                return true;
            }
            let haystack = if lines.len() == 1 {
                lines[0].as_str()
            } else {
                let mut s = String::new();
                for line in lines {
                    s.push_str(&line.as_str());
                }
                Cow::Owned(s)
            };
            let stable_idx = sr.start;

            if haystack.is_empty() {
                return true;
            }

            let haystack = match &pattern {
                CompiledPattern::CaseInSensitiveString(_) => Cow::Owned(haystack.to_lowercase()),
                _ => haystack,
            };
            let mut coords = None;

            match &pattern {
                CompiledPattern::CaseInSensitiveString(s)
                | CompiledPattern::CaseSensitiveString(s) => {
                    for (idx, s) in haystack.match_indices(s) {
                        found_match(
                            s,
                            idx,
                            lines,
                            stable_idx,
                            &mut uniq_matches,
                            &mut coords,
                            &mut results,
                        );
                    }
                }
                CompiledPattern::Regex(re) => {
                    // Allow for the regex to contain captures
                    for capture_res in re.captures_iter(&haystack) {
                        if let Ok(c) = capture_res {
                            // Look for the captures in reverse order, as index==0 is
                            // the whole matched string.  We can't just call
                            // `c.iter().rev()` as the capture iterator isn't double-ended.
                            for idx in (0..c.len()).rev() {
                                if let Some(m) = c.get(idx) {
                                    found_match(
                                        m.as_str(),
                                        m.start(),
                                        lines,
                                        stable_idx,
                                        &mut uniq_matches,
                                        &mut coords,
                                        &mut results,
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            // Keep iterating
            true
        });

        #[derive(Copy, Clone, Debug)]
        struct Coord {
            byte_idx: usize,
            grapheme_idx: usize,
            stable_row: StableRowIndex,
        }

        fn found_match(
            text: &str,
            byte_idx: usize,
            lines: &[&Line],
            stable_idx: StableRowIndex,
            uniq_matches: &mut HashMap<String, usize>,
            coords: &mut Option<Vec<Coord>>,
            results: &mut Vec<SearchResult>,
        ) {
            if coords.is_none() {
                coords.replace(make_coords(lines, stable_idx));
            }
            let coords = coords.as_ref().unwrap();

            let match_id = match uniq_matches.get(text).copied() {
                Some(id) => id,
                None => {
                    let id = uniq_matches.len();
                    uniq_matches.insert(text.to_owned(), id);
                    id
                }
            };
            let (start_x, start_y) = haystack_idx_to_coord(byte_idx, coords);
            let (end_x, end_y) = haystack_idx_to_coord(byte_idx + text.len(), coords);
            results.push(SearchResult {
                start_x,
                start_y,
                end_x,
                end_y,
                match_id,
            });
        }

        fn make_coords(lines: &[&Line], stable_row: StableRowIndex) -> Vec<Coord> {
            let mut byte_idx = 0;
            let mut coords = vec![];

            for (row_idx, line) in lines.iter().enumerate() {
                for cell in line.visible_cells() {
                    coords.push(Coord {
                        byte_idx,
                        grapheme_idx: cell.cell_index(),
                        stable_row: stable_row + row_idx as StableRowIndex,
                    });
                    byte_idx += cell.str().len();
                }
            }

            coords
        }

        fn haystack_idx_to_coord(idx: usize, coords: &[Coord]) -> (usize, StableRowIndex) {
            let c = coords
                .binary_search_by(|ele| ele.byte_idx.cmp(&idx))
                .or_else(|i| -> Result<usize, usize> { Ok(i) })
                .unwrap();
            let coord = coords.get(c).map(|c| *c).unwrap_or_else(|| {
                let last = coords.last().unwrap();
                Coord {
                    grapheme_idx: last.grapheme_idx + 1,
                    ..*last
                }
            });
            (coord.grapheme_idx, coord.stable_row)
        }

        Ok(results)
    }
}

struct LocalPaneDCSHandler {
    pane_id: PaneId,
    tmux_domain: Option<Arc<TmuxDomainState>>,
}

pub(crate) fn emit_output_for_pane(pane_id: PaneId, message: &str) {
    let mut parser = termwiz::escape::parser::Parser::new();
    let mut actions = vec![Action::CSI(CSI::Sgr(Sgr::Reset))];
    parser.parse(message.as_bytes(), |action| actions.push(action));

    promise::spawn::spawn_into_main_thread(async move {
        let mux = Mux::get();
        if let Some(pane) = mux.get_pane(pane_id) {
            pane.perform_actions(actions);
            mux.notify(MuxNotification::PaneOutput(pane_id));
        }
    })
    .detach();
}

impl wezterm_term::DeviceControlHandler for LocalPaneDCSHandler {
    fn handle_device_control(&mut self, control: termwiz::escape::DeviceControlMode) {
        match control {
            DeviceControlMode::Enter(mode) => {
                if !mode.ignored_extra_intermediates
                    && mode.params.len() == 1
                    && mode.params[0] == 1000
                    && mode.intermediates.is_empty()
                {
                    log::info!("tmux -CC mode requested");

                    // Create a new domain to host these tmux tabs
                    let domain = TmuxDomain::new(self.pane_id);
                    let tmux_domain = Arc::clone(&domain.inner);

                    let domain: Arc<dyn Domain> = Arc::new(domain);
                    let mux = Mux::get();
                    mux.add_domain(&domain);

                    if let Some(pane) = mux.get_pane(self.pane_id) {
                        let pane = pane.downcast_ref::<LocalPane>().unwrap();
                        pane.tmux_domain.lock().replace(Arc::clone(&tmux_domain));

                        emit_output_for_pane(
                            self.pane_id,
                            "\r\n[This pane is running tmux control mode. Press q to detach]",
                        );
                    }

                    self.tmux_domain.replace(tmux_domain);

                // TODO: do we need to proactively list available tabs here?
                // if so we should arrange to call domain.attach() and make
                // it do the right thing.
                } else if configuration().log_unknown_escape_sequences {
                    log::warn!("unknown DeviceControlMode::Enter {:?}", mode,);
                }
            }
            DeviceControlMode::Exit => {
                if let Some(tmux) = self.tmux_domain.take() {
                    let mux = Mux::get();
                    if let Some(pane) = mux.get_pane(self.pane_id) {
                        let pane = pane.downcast_ref::<LocalPane>().unwrap();
                        pane.tmux_domain.lock().take();
                    }
                    mux.domain_was_detached(tmux.domain_id);
                }
            }
            DeviceControlMode::Data(c) => {
                if configuration().log_unknown_escape_sequences {
                    log::warn!(
                        "unhandled DeviceControlMode::Data {:x} {}",
                        c,
                        (c as char).escape_debug()
                    );
                }
            }
            DeviceControlMode::TmuxEvents(events) => {
                if let Some(tmux) = self.tmux_domain.as_ref() {
                    tmux.advance(events);
                } else {
                    log::warn!("unhandled DeviceControlMode::TmuxEvents {:?}", &events);
                }
            }
            _ => {
                if configuration().log_unknown_escape_sequences {
                    log::warn!("unhandled: {:?}", control);
                }
            }
        }
    }
}

struct LocalPaneNotifHandler {
    pane_id: PaneId,
}

impl AlertHandler for LocalPaneNotifHandler {
    fn alert(&mut self, alert: Alert) {
        let pane_id = self.pane_id;
        promise::spawn::spawn_into_main_thread(async move {
            let mux = Mux::get();
            match &alert {
                Alert::WindowTitleChanged(title) => {
                    if let Some((_domain, window_id, _tab_id)) = mux.resolve_pane_id(pane_id) {
                        if let Some(mut window) = mux.get_window_mut(window_id) {
                            window.set_title(title);
                        }
                    }
                }
                Alert::TabTitleChanged(title) => {
                    if let Some((_domain, _window_id, tab_id)) = mux.resolve_pane_id(pane_id) {
                        if let Some(tab) = mux.get_tab(tab_id) {
                            tab.set_title(title.as_deref().unwrap_or(""));
                        }
                    }
                }
                _ => {}
            }

            mux.notify(MuxNotification::Alert { pane_id, alert });
        })
        .detach();
    }
}

/// This is a little gross; on some systems, our pipe reader will continue
/// to be blocked in read even after the child process has died.
/// We need to wake up and notice that the child terminated in order
/// for our state to wind down.
fn pane_exit(status: &ExitStatus) -> thinkterm_proto::PaneExit {
    thinkterm_proto::PaneExit {
        code: status.exit_code_if_known(),
        signal: status.signal().map(str::to_string),
    }
}

/// Whether `status` is a clean exit for `exit_behavior`. A pane a handoff
/// adopted ends with no status to judge by, and closes as though it exited
/// cleanly: a plain `exit` there should not read as a failure.
fn exited_cleanly(status: &ExitStatus, clean_exit_codes: &[u32]) -> bool {
    status.success()
        || status
            .exit_code_if_known()
            .map_or(true, |code| clean_exit_codes.contains(&code))
}

/// This block schedules a background thread to wait for the child
/// to terminate, and then nudge the muxer to check for dead processes.
/// Without this, typing `exit` in `cmd.exe` would keep the pane around
/// until something else triggered the mux to prune dead processes.
fn split_child(
    mut process: Box<dyn Child>,
    pane_id: PaneId,
) -> (
    Receiver<IoResult<ExitStatus>>,
    Box<dyn ChildKiller + Sync>,
    Option<u32>,
) {
    let pid = process.process_id();
    let signaller = process.clone_killer();

    let (tx, rx) = bounded(1);

    std::thread::spawn(move || {
        let status = process.wait();
        // Given to the mux as the program is reaped, not when the pane is
        // next asked whether it is dead: a pane removed by then is never
        // asked, and `cli wait` would never learn how it ended.
        if let (Ok(status), Some(mux)) = (&status, Mux::try_get()) {
            mux.record_pane_exit(pane_id, pane_exit(status));
        }
        tx.try_send(status).ok();
        promise::spawn::spawn_into_main_thread(async move {
            let mux = Mux::get();
            mux.prune_dead_windows();
        })
        .detach();
    });

    (rx, signaller, pid)
}

/// What a pane hands to a successor server besides its terminal: see
/// `LocalPane::handoff_parts`.
#[cfg(unix)]
pub struct PaneHandoffParts {
    pub pid: u32,
    pub tty_name: Option<std::path::PathBuf>,
    /// A duplicate of the master pty, taken under the pty lock: the pane
    /// may close its own between now and the send, and a bare number
    /// would then name whatever the kernel handed out next.
    pub pty_fd: std::os::fd::OwnedFd,
    pub description: String,
}

/// What a pane hands to a successor server besides its terminal, on
/// Windows: see `LocalPane::handoff_parts`.
#[cfg(windows)]
pub struct PaneHandoffParts {
    pub pid: u32,
    /// Waitable, killable, and enough to read the exit code.
    pub process: std::os::windows::io::OwnedHandle,
    pub conpty: portable_pty::win::conpty::ConPtyHandoff,
    pub description: String,
}

#[cfg(windows)]
fn open_child_process(pid: u32) -> std::io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::FromRawHandle;
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::winnt::{PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, SYNCHRONIZE};
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle as _) })
}

impl LocalPane {
    pub fn quiesce_connect_input(&self) -> anyhow::Result<()> {
        self.pty.lock().quiesce_input()
    }

    pub(crate) fn connect_eligible(&self) -> thinkterm_reverse_access::Result<()> {
        use thinkterm_reverse_access::Error;
        if self.tmux_domain.try_lock().ok_or(Error::Busy)?.is_some() {
            return Err(Error::Unavailable);
        }
        match &*self.process.try_lock().ok_or(Error::Busy)? {
            ProcessState::Running {
                pid: Some(_),
                killed: false,
                ..
            } => Ok(()),
            _ => Err(Error::Unavailable),
        }
    }

    pub(crate) fn connect_read(
        &self,
        options: thinkterm_reverse_access::protocol::ReadOptions,
    ) -> thinkterm_reverse_access::Result<String> {
        let term = self
            .terminal
            .try_lock()
            .ok_or(thinkterm_reverse_access::Error::Busy)?;
        connect_plain_text(&term, options)
    }

    pub(crate) fn connect_title(&self) -> thinkterm_reverse_access::Result<String> {
        let term = self
            .terminal
            .try_lock()
            .ok_or(thinkterm_reverse_access::Error::Busy)?;
        Ok(thinkterm_reverse_access::protocol::display_text(
            term.get_title(),
            128,
        ))
    }

    pub(crate) fn connect_input(
        &self,
        input: &crate::connect::Input,
    ) -> thinkterm_reverse_access::Result<(
        Box<dyn portable_pty::TryInputWriter>,
        crate::connect::EncodedInput,
    )> {
        use crate::connect::{EncodedInput, Input};
        use thinkterm_reverse_access::{Error, MAX_INPUT_BYTES, MAX_KEYS};
        let term = self.terminal.try_lock().ok_or(Error::Busy)?;
        let (mut bytes, enter, paste) = match input {
            Input::Text {
                text,
                no_paste,
                enter,
            } => {
                if text.len() > MAX_INPUT_BYTES {
                    return Err(Error::TooLarge);
                }
                let paste = !no_paste && term.bracketed_paste_enabled();
                let bytes = if *no_paste {
                    text.clone().into_bytes()
                } else {
                    term.encode_paste(text).into_bytes()
                };
                (bytes, *enter, paste)
            }
            Input::Keys(keys) => {
                if keys.is_empty() || keys.len() > MAX_KEYS {
                    return Err(Error::TooLarge);
                }
                let mut bytes = Vec::new();
                for (key, modifiers) in keys {
                    let encoded = term
                        .encode_key(key.clone(), *modifiers, true)
                        .map_err(|_| Error::Protocol)?;
                    if encoded.len() > MAX_INPUT_BYTES - bytes.len() {
                        return Err(Error::TooLarge);
                    }
                    bytes.extend_from_slice(encoded.as_bytes());
                }
                (bytes, false, false)
            }
        };
        let text_end = bytes.len();
        if enter {
            let key = term
                .encode_key(KeyCode::Enter, KeyModifiers::NONE, true)
                .map_err(|_| Error::Protocol)?;
            bytes.extend_from_slice(key.as_bytes());
        }
        let writer = self
            .pty
            .try_lock()
            .ok_or(Error::Busy)?
            .try_input_writer()
            .map_err(|_| Error::Unavailable)?;
        Ok((
            writer,
            EncodedInput {
                bytes,
                text_end,
                enter,
                enter_delay: thinkterm_reverse_access::protocol::enter_delay(text_end),
                paste,
            },
        ))
    }

    /// Takes `submit` only once the terminal is free, so Busy can be retried.
    pub(crate) fn connect_queue(
        &self,
        submit: &mut Option<Box<dyn FnOnce() + Send>>,
    ) -> thinkterm_reverse_access::Result<()> {
        use thinkterm_reverse_access::Error;
        let mut term = self.terminal.try_lock().ok_or(Error::Busy)?;
        term.queue_checked_input(submit.take().ok_or(Error::Protocol)?)
            .map_err(|_| Error::Unavailable)
    }

    /// The pieces a successor server needs to adopt this pane, or None
    /// for a pane that cannot travel: one over a channel rather than a
    /// pty (ssh, tmux), or one whose process has already exited.
    #[cfg(unix)]
    pub fn handoff_parts(&self) -> Option<PaneHandoffParts> {
        if self.tmux_domain.lock().is_some() {
            return None;
        }
        let pid = match &*self.process.lock() {
            ProcessState::Running { pid: Some(pid), .. } => *pid,
            _ => return None,
        };
        let pty = self.pty.lock();
        let raw = pty.as_raw_fd()?;
        let dup = unsafe { libc::dup(raw) };
        if dup < 0 {
            return None;
        }
        let pty_fd = unsafe {
            use std::os::fd::FromRawFd;
            std::os::fd::OwnedFd::from_raw_fd(dup)
        };
        Some(PaneHandoffParts {
            pid,
            tty_name: pty.tty_name(),
            pty_fd,
            description: self.command_description.clone(),
        })
    }

    /// The Windows sibling of the above: the pseudo console's handles in
    /// place of the pty descriptor, and the process itself, which a
    /// successor can wait on only with a handle. Err for a pane that should
    /// travel and whose parts could not be taken; the caller decides.
    #[cfg(windows)]
    pub fn handoff_parts(&self) -> Option<anyhow::Result<PaneHandoffParts>> {
        if self.tmux_domain.lock().is_some() {
            return None;
        }
        let pid = match &*self.process.lock() {
            ProcessState::Running { pid: Some(pid), .. } => *pid,
            _ => return None,
        };
        Some((|| {
            // Its waiter holds a handle until it exits, so the pid names
            // this process and no other.
            let process = open_child_process(pid)
                .map_err(|err| anyhow::anyhow!("opening process {pid}: {err}"))?;
            let conpty = self.pty.lock().conpty_handoff()?;
            Ok(PaneHandoffParts {
                pid,
                process,
                conpty,
                description: self.command_description.clone(),
            })
        })())
    }

    /// Forget the process without touching it: this pane was staged for
    /// a takeover that did not happen, and the process still belongs to
    /// the server that is keeping it. Dropping the pane afterwards sends
    /// nothing.
    pub fn release_process(&self) {
        *self.process.lock() = ProcessState::Dead;
    }

    /// Everything the terminal remembers. Take it with the reader paused
    /// (`crate::pause_pane_reader`), or it is a moment behind the pty.
    pub fn snapshot_terminal(
        &self,
    ) -> (
        wezterm_term::TerminalSnapshot,
        wezterm_term::KittyGraphicsSnapshot,
        wezterm_term::ShellCommandsSnapshot,
    ) {
        let mut term = self.terminal.lock();
        // A resize, configuration or focus noted while the parser held
        // the terminal is the parser's to apply; with the reader parked it
        // never will, so apply it here or the snapshot lags the pty.
        self.apply_pending_and_store(&mut term);
        (
            term.snapshot(),
            term.snapshot_kitty_graphics(),
            term.snapshot_shell_commands(),
        )
    }

    /// The last `max` rows of the primary screen with its scrollback,
    /// oldest first, whichever screen is showing: what the shell printed,
    /// not a full-screen program's frame.
    pub fn primary_screen_lines(&self, max: usize) -> Vec<Line> {
        let term = self.terminal.lock();
        let screen = term.screen_for_alt(false);
        let rows = screen.scrollback_rows();
        screen.lines_in_phys_range(rows.saturating_sub(max)..rows)
    }

    pub fn new(
        pane_id: PaneId,
        mut terminal: Terminal,
        process: Box<dyn Child + Send>,
        pty: Box<dyn MasterPty>,
        writer: Box<dyn Write + Send>,
        domain_id: DomainId,
        command_description: String,
    ) -> Self {
        let (process, signaller, pid) = split_child(process, pane_id);

        terminal.set_device_control_handler(Box::new(LocalPaneDCSHandler {
            pane_id,
            tmux_domain: None,
        }));
        terminal.set_notification_handler(Box::new(LocalPaneNotifHandler { pane_id }));
        let pty = Arc::new(Mutex::new(pty));
        #[cfg(unix)]
        {
            let source = Arc::clone(&pty);
            terminal.set_program_status_process_group(Box::new(move || {
                source.lock().process_group_leader().map(|pid| pid as u32)
            }));
        }
        let last_summary = Mutex::new(PaneSummary::from_terminal(&mut terminal, false));
        let config = Mutex::new(terminal.get_config());
        // Not zero for a terminal a handoff restored.
        let last_shell_commands = Mutex::new(terminal.shell_commands());

        Self {
            pane_id,
            terminal: Mutex::new(terminal),
            process: Mutex::new(ProcessState::Running {
                child_waiter: process,
                pid,
                signaller,
                killed: false,
            }),
            pty,
            writer: Mutex::new(writer),
            domain_id,
            tmux_domain: Mutex::new(None),
            proc_list: Mutex::new(None),
            #[cfg(unix)]
            leader: Arc::new(Mutex::new(None)),
            command_description,
            last_summary,
            pending_resize: Mutex::new(None),
            config,
            pending_config: Mutex::new(None),
            pending_focus: Mutex::new(None),
            pending_seen: Mutex::new(false),
            shell_idle: Mutex::new(None),
            last_progress: Mutex::new(Progress::default()),
            batch_reads: Mutex::new(Vec::new()),
            last_shell_commands,
            exit: Mutex::new(None),
        }
    }

    /// Tell the terminal of a focus change `focus_changed` could not, with
    /// the terminal lock held. See `apply_pending_resize`.
    fn apply_pending_focus(&self, term: &mut Terminal) {
        let pending = self.pending_focus.lock().take();
        if let Some(focused) = pending {
            term.focus_changed(focused);
        }
        if std::mem::take(&mut *self.pending_seen.lock()) {
            self.retire_seen_results(term);
        }
    }

    /// Someone has looked: retire the finished or failed results programs
    /// reported, and have the pane re-judged at once if any went. Asked by
    /// a frontend that showed the pane to a person -- never by focus alone,
    /// which code also moves with nobody looking.
    fn retire_seen_results(&self, term: &mut Terminal) {
        if term.program_status_seen() {
            crate::agent_status::nudge(self.pane_id);
        }
    }

    /// Everything the terminal has yet to take, applied under the guard
    /// the caller holds, and the stored copies refreshed from it.
    fn apply_pending_and_store(&self, term: &mut Terminal) {
        self.apply_pending_config(term);
        self.apply_pending_resize(term);
        self.apply_pending_focus(term);
        self.store_summary(term);
        *self.last_progress.lock() = term.get_progress();
        *self.last_shell_commands.lock() = term.shell_commands();
    }

    fn anything_pending(&self) -> bool {
        self.pending_resize.lock().is_some()
            || self.pending_config.lock().is_some()
            || self.pending_focus.lock().is_some()
            || *self.pending_seen.lock()
    }

    /// Give the terminal a configuration `set_config` could not, with the
    /// terminal lock held. See `apply_pending_resize`.
    fn apply_pending_config(&self, term: &mut Terminal) {
        let pending = self.pending_config.lock().take();
        if let Some(config) = pending {
            term.set_config(config);
        }
    }

    /// The summary the way it is stored: straight from the terminal, with
    /// nothing that needs a process lookup. `spice_summary` adds that at
    /// read time, so the parser thread never does it.
    fn store_summary(&self, term: &mut Terminal) -> PaneSummary {
        let summary = PaneSummary::from_terminal(term, self.tmux_domain.lock().is_some());
        *self.last_summary.lock() = summary.clone();
        summary
    }

    /// What `get_title` and `get_current_working_dir` add beyond the
    /// terminal's own answer, from the cached process list and never from
    /// the terminal. `listing_summary` is the summary with these on.
    fn spice_summary(&self, summary: &mut PaneSummary) {
        self.spice_title(&mut summary.title);
        summary.working_dir =
            self.working_dir_or_divined(summary.working_dir.take(), CachePolicy::AllowStale);
    }

    /// A pane that never set a title is named after its foreground process.
    fn spice_title(&self, title: &mut String) {
        if title != "thinkterm" {
            return;
        }
        if let Some(proc_name) = self.get_foreground_process_name(CachePolicy::AllowStale) {
            if let Some(name) = std::path::Path::new(&proc_name).file_name() {
                *title = name.to_string_lossy().to_string();
            }
        }
    }

    /// The directory the shell reported, or the one divined from the
    /// foreground process when it reported none.
    fn working_dir_or_divined(&self, reported: Option<Url>, policy: CachePolicy) -> Option<Url> {
        reported.or_else(|| self.divine_current_working_dir(policy))
    }

    /// Give the terminal a size the pty already has. Called with the
    /// terminal lock held, before and after a batch of output: before, so
    /// output written in answer to the resize is parsed at the new size;
    /// after, for a resize that arrived during the batch.
    fn apply_pending_resize(&self, term: &mut Terminal) {
        // The note's lock is not held across the resize: a resize
        // reflows the scrollback, and `summary_without_waiting` reads
        // the note on the thread that must not wait. Nor is the note
        // taken before the resize: until the stored snapshot shows the
        // new size, a reader must still find the note, or it would take
        // the old snapshot for the size the pane has. So the note stays
        // until the snapshot is stored, and goes only if it is still the
        // size just applied; a newer one waits for the next look.
        let pending = *self.pending_resize.lock();
        if let Some(size) = pending {
            term.resize(size);
            self.store_summary(term);
            let mut note = self.pending_resize.lock();
            if *note == Some(size) {
                *note = None;
            }
        }
    }

    #[cfg(unix)]
    fn get_leader(&self, policy: CachePolicy) -> CachedLeaderInfo {
        let mut leader = self.leader.lock();

        if policy == CachePolicy::FetchImmediate || leader.is_none() {
            // The fd is bound first so the pty guard drops before the
            // probe syscalls run; inlined, the temporary guard lives to
            // the end of the statement and blocks resize/tty_name/writes
            // for the probe's full ~700µs+.
            let fd = self.pty.lock().as_raw_fd();
            let claim = leader
                .as_ref()
                .map(|info| info.claim.wrapping_add(1))
                .unwrap_or(0);
            leader.replace(CachedLeaderInfo::new(fd, claim));
        } else if let Some(info) = leader.as_mut() {
            // If stale, claim the entry and queue a background refresh;
            // the stale data is returned immediately.
            if info.expired() && info.can_update() {
                info.updating = true;
                info.claim = info.claim.wrapping_add(1);
                leader_refresh::request_refresh(&self.leader);
            }
        }

        (*leader).clone().unwrap()
    }

    fn divine_current_working_dir(&self, policy: CachePolicy) -> Option<Url> {
        #[cfg(unix)]
        {
            let leader = self.get_leader(policy);
            if let Some(path) = &leader.current_working_dir {
                return Url::from_directory_path(path).ok();
            }
            return None;
        }

        #[cfg(windows)]
        if let Some(fg) = self.divine_foreground_process(policy) {
            return Url::from_directory_path(fg.cwd).ok();
        }

        #[allow(unreachable_code)]
        None
    }

    fn divine_process_list(
        &self,
        policy: CachePolicy,
    ) -> Option<MappedMutexGuard<'_, CachedProcInfo>> {
        if let ProcessState::Running { pid: Some(pid), .. } = &*self.process.lock() {
            let mut proc_list = self.proc_list.lock();

            let expired = policy == CachePolicy::FetchImmediate
                || proc_list
                    .as_ref()
                    .map(|info| info.updated.elapsed() > PROC_INFO_CACHE_TTL)
                    .unwrap_or(true);

            if expired {
                log::trace!("CachedProcInfo expired, refresh");
                let root = LocalProcessInfo::with_root_pid(*pid)?;

                // Windows doesn't have any job control or session concept,
                // so we infer that the equivalent to the process group
                // leader is the most recently spawned program running
                // in the console
                let mut youngest = &root;

                fn find_youngest<'a>(
                    proc: &'a LocalProcessInfo,
                    youngest: &mut &'a LocalProcessInfo,
                ) {
                    if proc.start_time >= youngest.start_time {
                        *youngest = proc;
                    }

                    for child in proc.children.values() {
                        #[cfg(windows)]
                        if child.console == 0 {
                            continue;
                        }
                        find_youngest(child, youngest);
                    }
                }

                find_youngest(&root, &mut youngest);
                let mut foreground = youngest.clone();
                foreground.children.clear();

                proc_list.replace(CachedProcInfo {
                    root,
                    foreground,
                    updated: Instant::now(),
                });
                log::trace!("CachedProcInfo updated");
            }

            return Some(MutexGuard::map(proc_list, |info| info.as_mut().unwrap()));
        }
        None
    }

    #[allow(dead_code)]
    fn divine_foreground_process(&self, policy: CachePolicy) -> Option<LocalProcessInfo> {
        if let Some(info) = self.divine_process_list(policy) {
            Some(info.foreground.clone())
        } else {
            None
        }
    }
}

impl Drop for LocalPane {
    fn drop(&mut self) {
        // Avoid lingering zombies if we can, but don't block forever.
        // <https://github.com/wezterm/wezterm/issues/558>
        if let ProcessState::Running { signaller, .. } = &mut *self.process.lock() {
            let _ = signaller.kill();
        }
    }
}

#[cfg(test)]
mod connect_read_tests {
    use super::*;
    use thinkterm_reverse_access::protocol::ReadOptions;

    #[derive(Debug)]
    struct TestConfig;
    impl TerminalConfiguration for TestConfig {
        fn scrollback_size(&self) -> usize {
            32
        }
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    fn terminal(cols: usize, rows: usize) -> Terminal {
        Terminal::new(
            TerminalSize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 96,
            },
            Arc::new(TestConfig),
            "ThinkTerm",
            "test",
            Box::new(Vec::new()),
        )
    }

    #[test]
    fn connect_plain_text_preserves_wrap_trim_and_inclusive_ranges() {
        let mut terminal = terminal(4, 4);
        terminal.advance_bytes(b"abcde\r\nx   \r\n");
        let trim = ReadOptions {
            trim: true,
            ..ReadOptions::default()
        };
        assert_eq!(connect_plain_text(&terminal, trim).unwrap(), "abcd\ne\nx\n");
        assert_eq!(
            connect_plain_text(
                &terminal,
                ReadOptions {
                    unwrap: true,
                    ..trim
                }
            )
            .unwrap(),
            "abcde\nx\n"
        );
        assert_eq!(
            connect_plain_text(
                &terminal,
                ReadOptions {
                    start: Some(1),
                    end: Some(1),
                    ..trim
                }
            )
            .unwrap(),
            "e\n"
        );
        assert!(connect_plain_text(
            &terminal,
            ReadOptions {
                start: Some(2),
                end: Some(1),
                ..trim
            }
        )
        .is_err());
    }

    #[test]
    fn connect_plain_text_uses_current_screen_coordinates_and_bounds_history() {
        let mut terminal = terminal(8, 2);
        terminal.advance_bytes(b"first\r\nsecond\r\nthird");
        let trim = ReadOptions {
            trim: true,
            ..ReadOptions::default()
        };
        assert_eq!(
            connect_plain_text(&terminal, trim).unwrap(),
            "second\nthird\n"
        );
        assert_eq!(
            connect_plain_text(
                &terminal,
                ReadOptions {
                    start: Some(-1),
                    end: Some(-1),
                    ..trim
                }
            )
            .unwrap(),
            "first\n"
        );
        terminal.advance_bytes(b"\x1b[?1049hALT");
        assert_eq!(
            connect_plain_text(
                &terminal,
                ReadOptions {
                    start: Some(-100),
                    ..trim
                }
            )
            .unwrap(),
            "ALT\n"
        );
        assert!(connect_plain_text(
            &terminal,
            ReadOptions {
                start: Some(i64::MAX),
                ..trim
            }
        )
        .is_err());
    }

    /// Once the scrollback is full the line store wraps around in memory,
    /// wherever a read's range falls in it.
    #[test]
    fn connect_plain_text_reads_a_full_scrollback() {
        let trim = ReadOptions {
            trim: true,
            ..ReadOptions::default()
        };
        let mut terminal = terminal(16, 4);
        for line in 0..2000 {
            terminal.advance_bytes(format!("line-{line}\r\n").as_bytes());
            if line < 40 {
                continue;
            }
            assert_eq!(
                connect_plain_text(&terminal, trim).unwrap(),
                format!("line-{}\nline-{}\nline-{}\n", line - 2, line - 1, line)
            );
            assert_eq!(
                connect_plain_text(
                    &terminal,
                    ReadOptions {
                        start: Some(-8),
                        end: Some(-1),
                        ..trim
                    }
                )
                .unwrap(),
                (line - 10..=line - 3)
                    .map(|line| format!("line-{line}\n"))
                    .collect::<String>()
            );
        }
    }

    #[test]
    fn connect_plain_text_rejects_a_view_over_the_line_limit() {
        let terminal = terminal(1, thinkterm_reverse_access::MAX_READ_LINES + 1);
        assert_eq!(
            connect_plain_text(&terminal, ReadOptions::default()),
            Err(thinkterm_reverse_access::Error::TooLarge)
        );
    }
}

#[cfg(all(test, unix))]
mod connect_host_tests {
    use super::*;
    use crate::client::ClientId;
    use crate::connect::{Host, HostRoute};
    use crate::tab::Tab;
    use crate::Mux;
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use smol::io::{AsyncRead, AsyncWrite};
    use std::future::Future;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use thinkterm_reverse_access::budget::Budgets;
    use thinkterm_reverse_access::channel::{Channel, Deadline};
    use thinkterm_reverse_access::exchange::receive_reply;
    use thinkterm_reverse_access::protocol::{Command, ExportRef, InputStatus, ReadOptions, Reply};
    use thinkterm_reverse_access::wire::{Assembly, Payload};
    use thinkterm_reverse_access::{Error, Id, Permission};

    /// Stands in for the native window, which needs a person: approves
    /// whatever it is asked, and keeps the session monitor's heartbeat.
    const HELPER: &str = r#"#!/bin/sh
case "$1" in
--monitor)
    printf '{"kind":"ready","id":null}\n'
    while :; do
        printf '{"kind":"alive","id":null}\n'
        sleep 1
    done
    ;;
--authenticate)
    IFS= read -r line
    id=${line#*\"Id\":\"}
    printf '{"kind":"approved","id":"%s"}\n' "${id%%\"*}"
    ;;
esac
"#;

    struct InstalledHelper(PathBuf);
    impl InstalledHelper {
        fn new() -> Self {
            let path = std::env::current_exe()
                .unwrap()
                .with_file_name("thinkterm-connect-auth");
            std::fs::write(&path, HELPER).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self(path)
        }
    }
    impl Drop for InstalledHelper {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[derive(Debug)]
    struct TestConfig;
    impl TerminalConfiguration for TestConfig {
        fn scrollback_size(&self) -> usize {
            32
        }
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    struct Fixture {
        mux: Arc<Mux>,
        pane: Arc<LocalPane>,
        connect: Arc<Host>,
        client: ClientId,
        route: Arc<HostRoute>,
        reader: Box<dyn Read + Send>,
        sequence: u64,
        _helper: InstalledHelper,
        _serial: parking_lot::MutexGuard<'static, ()>,
    }

    fn fixture() -> Fixture {
        static SCHEDULER: std::sync::Once = std::sync::Once::new();
        SCHEDULER.call_once(|| {
            promise::spawn::SimpleExecutor::new();
        });
        // Which also keeps these to the helper's one path, one at a time.
        let serial = crate::GLOBAL_MUX_TESTS.lock();
        let helper = InstalledHelper::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let size = TerminalSize {
            cols: 40,
            rows: 6,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 96,
        };
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 6,
                cols: 40,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let child = pair
            .slave
            .spawn_command(CommandBuilder::new("/bin/cat"))
            .unwrap();
        let reader = pair.master.try_clone_reader().unwrap();
        let terminal = Terminal::new(
            size,
            Arc::new(TestConfig),
            "ThinkTerm",
            "test",
            pair.master.take_writer().unwrap(),
        );
        let pane = Arc::new(LocalPane::new(
            crate::pane::alloc_pane_id(),
            terminal,
            child,
            pair.master,
            Box::new(Vec::new()),
            0,
            "test".into(),
        ));
        let dyn_pane: Arc<dyn Pane> = pane.clone();
        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(&dyn_pane);
        mux.panes.write().insert(pane.pane_id(), dyn_pane);
        mux.add_tab_no_panes(&tab);
        let mut window = crate::window::Window::new(Some("default".into()), None, None);
        window.push(&tab);
        mux.windows.write().insert(window.window_id(), window);
        let client = ClientId {
            hostname: "devbox".into(),
            username: "user".into(),
            pid: std::process::id(),
            epoch: 0,
            id: 0,
            ssh_auth_sock: None,
        };
        mux.register_client(Arc::new(client.clone()));
        let connect = Host::new(&mux).unwrap();
        let route = connect
            .open(client.clone(), "user@server-a:22", "zh-CN")
            .unwrap();
        route.heartbeat().unwrap();
        route
            .begin_all_local_approval(Permission::ReadAndInput, 60, None)
            .unwrap();
        let deadline = Deadline::request().unwrap();
        while route.status().grants.is_empty() {
            assert!(route.status().error.is_none());
            deadline.remaining().unwrap();
            route.heartbeat().unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        Fixture {
            mux,
            pane,
            connect,
            client,
            route,
            reader,
            sequence: 0,
            _helper: helper,
            _serial: serial,
        }
    }

    impl Fixture {
        /// One request through the host and over a real socket, as the data
        /// connection carries it.
        fn exchange(
            &mut self,
            command: Command,
            payload: Option<Payload>,
        ) -> thinkterm_reverse_access::Result<(Reply, Option<Payload>)> {
            self.sequence += 1;
            let sequence = self.sequence;
            let route = Arc::clone(&self.route);
            route.heartbeat()?;
            smol::block_on(async move {
                let budgets = Budgets::new()?;
                let budget = budgets.open_route(Id::random()?)?;
                let (near, far) = smol::Async::<UnixStream>::pair().unwrap();
                let mut near = Channel::new(near, budget.acquire(1024 * 1024)?)?;
                let mut far = Channel::new(far, budget.acquire(1024 * 1024)?)?;
                let deadline = Deadline::request()?;
                let reply = route
                    .prepare_reply(sequence, command, payload, &deadline, &deadline)
                    .await?;
                let (sent, received) = smol::future::zip(
                    reply.send(&mut near, sequence, &deadline, &deadline),
                    receive_reply(
                        &mut far,
                        sequence,
                        |bytes| Assembly::text(bytes, &budget),
                        &deadline,
                    ),
                )
                .await;
                sent?;
                received?
            })
        }

        fn list(&mut self) -> Vec<ExportRef> {
            match self
                .exchange(Command::List { after: None }, None)
                .unwrap()
                .0
            {
                Reply::List { exports, .. } => exports.iter().map(|item| item.export).collect(),
                _ => panic!("not a list"),
            }
        }

        fn read(&mut self, export: ExportRef) -> String {
            let command = Command::Read {
                export,
                options: ReadOptions {
                    trim: true,
                    ..ReadOptions::default()
                },
            };
            let (reply, payload) = self.exchange(command, None).unwrap();
            assert!(matches!(reply, Reply::Text { .. }));
            payload.unwrap().text().to_string()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.route.close();
            self.mux.panes.write().clear();
        }
    }

    /// Takes a frame's first byte and then nothing until `opens`, as a
    /// connection that has slowed down leaves a frame partly written.
    struct Stalled {
        socket: smol::Async<UnixStream>,
        opens: smol::Timer,
        started: bool,
        open: bool,
    }
    impl AsyncRead for Stalled {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.socket).poll_read(cx, bytes)
        }
    }
    impl AsyncWrite for Stalled {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if !self.started {
                self.started = true;
                return Pin::new(&mut self.socket).poll_write(cx, &bytes[..1]);
            }
            if !self.open {
                if Pin::new(&mut self.opens).poll(cx).is_pending() {
                    return Poll::Pending;
                }
                self.open = true;
            }
            Pin::new(&mut self.socket).poll_write(cx, bytes)
        }
        fn poll_flush(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.socket).poll_flush(cx)
        }
        fn poll_close(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.socket).poll_close(cx)
        }
    }

    #[test]
    fn a_grant_lists_reads_and_types_into_a_real_terminal() {
        let mut host = fixture();
        let exports = host.list();
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].target.pane, host.pane.pane_id() as u64);

        // Past the scrollback, where a read used to end the whole host.
        {
            let mut terminal = host.pane.terminal.lock();
            for line in 0..500 {
                terminal.advance_bytes(format!("line-{line}\r\n").as_bytes());
            }
        }
        assert!(host.read(exports[0]).ends_with("line-499\n"));

        // A terminal in the middle of its output is waited for, not failed.
        let pane = Arc::clone(&host.pane);
        let (locked, is_locked) = std::sync::mpsc::channel();
        let parser = std::thread::spawn(move || {
            let terminal = pane.terminal.lock();
            locked.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            drop(terminal);
        });
        is_locked.recv().unwrap();
        assert!(host.read(exports[0]).ends_with("line-499\n"));
        parser.join().unwrap();
        let pane = Arc::clone(&host.pane);
        let (locked, is_locked) = std::sync::mpsc::channel();
        let parser = std::thread::spawn(move || {
            let terminal = pane.terminal.lock();
            locked.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            drop(terminal);
        });
        is_locked.recv().unwrap();
        assert_eq!(host.list(), exports);
        parser.join().unwrap();

        // Text, then its Enter after the delay, reach the program.
        let started = Instant::now();
        let budgets = Budgets::new().unwrap();
        let budget = budgets.open_route(Id::random().unwrap()).unwrap();
        let mut payload = Assembly::input(5, &budget).unwrap();
        payload.append(0, b"hello").unwrap();
        let (reply, _) = host
            .exchange(
                Command::Text {
                    export: exports[0],
                    bytes: 5,
                    no_paste: true,
                    enter: true,
                },
                Some(payload.finish().unwrap()),
            )
            .unwrap();
        let Reply::Input(result) = reply else {
            panic!("not an input result");
        };
        assert_eq!(result.status, InputStatus::Submitted);
        assert!(result.enter_submitted);
        assert!(started.elapsed() >= thinkterm_reverse_access::protocol::enter_delay(5));
        let mut seen = Vec::new();
        let mut bytes = [0; 256];
        while !String::from_utf8_lossy(&seen).contains("hello") {
            let count = host.reader.read(&mut bytes).unwrap();
            assert!(count > 0);
            seen.extend_from_slice(&bytes[..count]);
        }

        // Revoked is over, for reads as for the next list.
        let grant = host.route.status().grants[0].grant;
        host.route.revoke(grant).unwrap();
        assert!(host
            .exchange(
                Command::Read {
                    export: exports[0],
                    options: ReadOptions::default(),
                },
                None,
            )
            .is_err());
        assert!(host.route.status().grants.is_empty());
    }

    #[test]
    fn a_read_out_of_time_partway_through_a_frame_finishes_it_and_cancels() {
        let mut host = fixture();
        let exports = host.list();
        host.pane.terminal.lock().advance_bytes(b"example\r\n");
        host.route.heartbeat().unwrap();
        host.sequence += 1;
        let sequence = host.sequence;
        let command = Command::Read {
            export: exports[0],
            options: ReadOptions::default(),
        };
        let route = Arc::clone(&host.route);
        let received = smol::block_on(async move {
            let budgets = Budgets::new()?;
            let budget = budgets.open_route(Id::random()?)?;
            let (near, far) = smol::Async::<UnixStream>::pair().unwrap();
            // Still stuck in the reply's first frame when the operation ends.
            let near = Stalled {
                socket: near,
                opens: smol::Timer::after(Duration::from_millis(600)),
                started: false,
                open: false,
            };
            let mut near = Channel::new(near, budget.acquire(1024 * 1024)?)?;
            let mut far = Channel::new(far, budget.acquire(1024 * 1024)?)?;
            let deadline = Deadline::request()?;
            let operation = Deadline::after(Duration::from_millis(400))?;
            let reply = route
                .prepare_reply(sequence, command, None, &operation, &deadline)
                .await?;
            let (sent, received) = smol::future::zip(
                reply.send(&mut near, sequence, &operation, &deadline),
                receive_reply(
                    &mut far,
                    sequence,
                    |bytes| Assembly::text(bytes, &budget),
                    &deadline,
                ),
            )
            .await;
            sent?;
            received
        })
        .unwrap();
        assert!(matches!(received, Err(Error::Unavailable)));

        // That was one request: the route and its grant carry on.
        assert!(host.read(exports[0]).contains("example"));
    }

    #[test]
    fn a_second_press_of_authenticate_is_the_approval_already_running() {
        let host = fixture();
        let grant = host.route.status().grants[0].grant;
        std::fs::write(
            &host._helper.0,
            "#!/bin/sh\nIFS= read -r line\nid=${line#*\\\"Id\\\":\\\"}\nsleep 1\n\
             printf '{\"kind\":\"approved\",\"id\":\"%s\"}\\n' \"${id%%\\\"*}\"\n",
        )
        .unwrap();
        for _ in 0..2 {
            host.route
                .begin_all_local_approval(Permission::Read, 60, Some(grant))
                .unwrap();
            let status = host.route.status();
            assert!(status.authenticating);
            assert!(status.error.is_none());
        }

        // Another route has not asked yet, and has to wait for this one.
        let other = host
            .connect
            .open(host.client.clone(), "user@server-b:22", "zh-CN")
            .unwrap();
        assert!(matches!(
            other.begin_all_local_approval(Permission::Read, 60, None),
            Err(Error::Busy)
        ));
        assert!(other.status().error.is_some());
        assert!(host.route.status().error.is_none());

        let deadline = Deadline::request().unwrap();
        while host.route.status().authenticating {
            deadline.remaining().unwrap();
            host.route.heartbeat().unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = host.route.status();
        assert!(status.error.is_none());
        assert_eq!(status.grants.len(), 1);
        assert_ne!(status.grants[0].grant, grant);
    }

    #[test]
    fn a_refused_dialog_is_not_a_failure_and_a_missing_one_stays_reported() {
        let host = fixture();
        let grant = host.route.status().grants[0].grant;
        std::fs::write(
            &host._helper.0,
            "#!/bin/sh\nIFS= read -r line\nid=${line#*\\\"Id\\\":\\\"}\n\
             printf '{\"kind\":\"rejected\",\"id\":\"%s\"}\\n' \"${id%%\\\"*}\"\n",
        )
        .unwrap();
        host.route
            .begin_all_local_approval(Permission::Read, 60, Some(grant))
            .unwrap();
        let deadline = Deadline::request().unwrap();
        while host.route.status().authenticating {
            deadline.remaining().unwrap();
            host.route.heartbeat().unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = host.route.status();
        assert!(status.error.is_none());
        assert_eq!(status.grants[0].grant, grant);

        std::fs::remove_file(&host._helper.0).unwrap();
        host.route.revoke(grant).unwrap();
        // The monitor ended with the last grant, so this needs a new helper.
        assert!(host
            .route
            .begin_all_local_approval(Permission::Read, 60, None)
            .is_err());
        assert!(host.route.status().error.is_some());
        assert!(host.route.status().error.is_some());
        host.route.cancel_approval();
        assert!(host.route.status().error.is_none());
    }
}

#[cfg(test)]
mod summary_tests {
    use super::*;
    use portable_pty::PtySize;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn pane_exit_preserves_unknown_and_known_statuses() {
        assert_eq!(pane_exit(&ExitStatus::unknown()).code, None);
        assert_eq!(pane_exit(&ExitStatus::with_exit_code(0)).code, Some(0));
        assert_eq!(pane_exit(&ExitStatus::with_exit_code(7)).code, Some(7));
        let killed = pane_exit(&ExitStatus::with_signal("Terminated"));
        assert_eq!(killed.code, Some(1));
        assert_eq!(killed.signal.as_deref(), Some("Terminated"));
    }

    #[test]
    fn an_unknown_status_closes_like_a_clean_exit() {
        assert!(exited_cleanly(&ExitStatus::unknown(), &[]));
        assert!(exited_cleanly(&ExitStatus::with_exit_code(0), &[]));
        assert!(!exited_cleanly(&ExitStatus::with_exit_code(3), &[]));
        assert!(exited_cleanly(&ExitStatus::with_exit_code(3), &[3]));
        assert!(!exited_cleanly(&ExitStatus::with_signal("Terminated"), &[]));
    }

    /// A child that never exits, so the waiter thread `LocalPane::new`
    /// spawns sits on it for the life of the test.
    #[derive(Debug)]
    struct SleepingChild(std::sync::mpsc::Receiver<()>);

    impl ChildKiller for SleepingChild {
        fn kill(&mut self) -> IoResult<()> {
            Ok(())
        }
        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(NoKiller)
        }
    }

    #[derive(Debug)]
    struct NoKiller;

    impl ChildKiller for NoKiller {
        fn kill(&mut self) -> IoResult<()> {
            Ok(())
        }
        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(NoKiller)
        }
    }

    impl Child for SleepingChild {
        fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
            Ok(None)
        }
        fn wait(&mut self) -> IoResult<ExitStatus> {
            let _ = self.0.recv();
            Ok(ExitStatus::with_exit_code(0))
        }
        fn process_id(&self) -> Option<u32> {
            None
        }
        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    /// A pty that remembers its sizes and reports a chosen foreground group.
    struct RecordingPty(Arc<StdMutex<Vec<PtySize>>>, Option<u32>);

    impl MasterPty for RecordingPty {
        fn resize(&self, size: PtySize) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(size);
            Ok(())
        }
        fn get_size(&self) -> anyhow::Result<PtySize> {
            Ok(self.0.lock().unwrap().last().copied().unwrap_or_default())
        }
        fn try_clone_reader(&self) -> anyhow::Result<Box<dyn std::io::Read + Send>> {
            anyhow::bail!("no reader")
        }
        fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
            anyhow::bail!("no writer")
        }
        #[cfg(unix)]
        fn process_group_leader(&self) -> Option<libc::pid_t> {
            self.1.map(|pid| pid as libc::pid_t)
        }
        #[cfg(unix)]
        fn as_raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
            None
        }
        #[cfg(unix)]
        fn tty_name(&self) -> Option<std::path::PathBuf> {
            None
        }
    }

    fn size(cols: usize, rows: usize) -> TerminalSize {
        TerminalSize {
            cols,
            rows,
            pixel_width: cols * 8,
            pixel_height: rows * 16,
            dpi: 96,
        }
    }

    /// The waiter thread `LocalPane::new` starts hops to the main thread
    /// when the child exits; with no scheduler that is a panic that
    /// poisons promise's scheduler lock for every test after.
    fn ensure_scheduler() {
        static SCHEDULER: std::sync::Once = std::sync::Once::new();
        SCHEDULER.call_once(|| {
            promise::spawn::SimpleExecutor::new();
        });
    }

    fn pane() -> (
        LocalPane,
        Arc<StdMutex<Vec<PtySize>>>,
        std::sync::mpsc::Sender<()>,
    ) {
        ensure_scheduler();
        let (exit_tx, exit_rx) = std::sync::mpsc::channel();
        let sizes = Arc::new(StdMutex::new(vec![]));
        let terminal = Terminal::new(
            size(80, 24),
            Arc::new(config::TermConfig::new()),
            "ThinkTerm",
            "test",
            Box::new(Vec::new()),
        );
        let pane = LocalPane::new(
            1,
            terminal,
            Box::new(SleepingChild(exit_rx)),
            Box::new(RecordingPty(Arc::clone(&sizes), None)),
            Box::new(Vec::new()),
            0,
            "test".into(),
        );
        (pane, sizes, exit_tx)
    }

    #[test]
    fn the_summary_is_answered_from_the_last_read_while_the_terminal_is_held() {
        let (pane, _sizes, _exit) = pane();
        let title_before = pane.summary_without_waiting().title;
        // The parser thread, mid-batch.
        let guard = pane.terminal.lock();
        let started = Instant::now();
        let summary = pane.summary_without_waiting();
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "answered without waiting for the lock"
        );
        assert_eq!(summary.title, title_before);
        assert_eq!(summary.dimensions.cols, 80);
        drop(guard);
    }

    /// The alternate screen numbers its rows from nought, so bounds taken
    /// on it name the primary screen's oldest history once the program
    /// has left it. Rows held to the screen are read with the screen.
    #[test]
    fn rows_held_to_the_screen_are_read_with_it() {
        let (pane, _sizes, _exit) = pane();
        let feed = |text: &str| {
            let mut parser = termwiz::escape::parser::Parser::new();
            let mut actions = vec![];
            parser.parse(text.as_bytes(), |action| actions.push(action));
            pane.perform_actions(actions);
        };
        for row in 0..60 {
            feed(&format!("history {row}\r\n"));
        }
        feed("\x1b[?1049hfull screen");
        let alt = pane.get_dimensions();
        let asked = alt.physical_top..alt.physical_top + alt.viewport_rows as StableRowIndex;
        let (_, lines) = pane.get_screen_lines(asked.clone());
        assert!(lines[0].as_str().starts_with("full screen"));

        // The program leaves its screen between the bounds and the rows.
        feed("\x1b[?1049l");
        let (_, unheld) = pane.get_lines(asked.clone());
        assert!(unheld[0].as_str().starts_with("history 0"));
        let (_, held) = pane.get_screen_lines(asked);
        assert!(held.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn program_status_keeps_the_process_group_observed_by_the_parser() {
        let (pane, sizes, _exit) = pane();
        let report = || {
            let mut parser = termwiz::escape::parser::Parser::new();
            let mut actions = vec![];
            parser.parse(b"\x1b]7501;state=working:app=example\x1b\\", |a| {
                actions.push(a)
            });
            actions
        };
        *pane.pty.lock() = Box::new(RecordingPty(Arc::clone(&sizes), Some(9101)));
        pane.perform_actions(report());
        *pane.pty.lock() = Box::new(RecordingPty(sizes, Some(9102)));
        let first = pane.program_status().root.unwrap();
        assert_eq!(first.process_group, Some(9101));
        pane.perform_actions(report());
        let second = pane.program_status().root.unwrap();
        assert_eq!(second.process_group, Some(9102));
        assert_ne!(second.serial, first.serial);
    }

    #[test]
    fn a_held_exited_pane_retires_active_reports_including_trailing_output() {
        use wezterm_term::program_status::ProgramState;
        let (pane, _sizes, _exit) = pane();
        let report = |state: &str, id: &str| {
            let mut parser = termwiz::escape::parser::Parser::new();
            let mut actions = vec![];
            let osc = format!("\x1b]7501;state={state}:id={id}:app=example\x1b\\");
            parser.parse(osc.as_bytes(), |action| actions.push(action));
            pane.perform_actions(actions);
        };
        report("working", "work");
        report("blocked", "input");
        report("done", "result");
        assert_eq!(pane.program_status().children.len(), 3);

        // Both Hold and a failed CloseOnCleanExit retain this pane state.
        *pane.process.lock() = ProcessState::DeadPendingClose { killed: false };
        assert!(!pane.is_dead());
        assert!(pane.root_process_id().is_none());
        let snapshot = pane.program_status();
        assert_eq!(snapshot.children.len(), 1);
        assert_eq!(snapshot.children[0].state, ProgramState::Done);
        assert!(snapshot.children[0].orphaned);

        // The reader may still be draining bytes after the child exits.
        report("working", "late-work");
        report("error", "late-result");
        let snapshot = pane.program_status();
        assert_eq!(snapshot.children.len(), 2);
        assert!(snapshot.children.iter().all(|child| child.orphaned));
        assert!(snapshot.children.iter().all(|child| {
            matches!(child.state, ProgramState::Done | ProgramState::Error)
        }));
    }

    #[test]
    fn a_resize_while_the_terminal_is_held_reaches_the_pty_now_and_the_terminal_later() {
        let (pane, sizes, _exit) = pane();
        let guard = pane.terminal.lock();
        pane.resize(size(100, 30)).unwrap();
        pane.resize(size(120, 40)).unwrap();
        assert_eq!(
            sizes
                .lock()
                .unwrap()
                .iter()
                .map(|s| s.cols)
                .collect::<Vec<_>>(),
            vec![100, 120],
            "the pty took both at once"
        );
        let summary = pane.summary_without_waiting();
        assert_eq!(
            summary.resize_pending.map(|s| s.cols),
            Some(120),
            "the summary notes the size the pane has"
        );
        assert_eq!(
            summary.dimensions.cols, 80,
            "next to a coherent snapshot of what the screen still shows"
        );
        assert_eq!(summary.terminal_size().cols, 120);
        assert_eq!(guard.get_size().cols, 80, "the terminal has neither yet");
        drop(guard);
        // The parser's next batch, empty as it may be.
        pane.perform_actions(vec![]);
        assert_eq!(
            pane.get_dimensions().cols,
            120,
            "and only the newest one lands"
        );
        assert!(pane.pending_resize.lock().is_none());
    }

    /// While the parser is applying a noted resize, a reader that cannot
    /// take the terminal still finds the note: the old snapshot without
    /// it would say the pane has its old size.
    #[test]
    fn the_note_outlives_the_reflow_until_the_snapshot_shows_the_size() {
        let (pane, _sizes, _exit) = pane();
        let guard = pane.terminal.lock();
        pane.resize(size(100, 30)).unwrap();
        drop(guard);
        // What the parser does, step by step: the resize itself first.
        let mut term = pane.terminal.lock();
        let before = *pane.pending_resize.lock();
        assert_eq!(before.map(|s| s.cols), Some(100));
        pane.apply_pending_resize(&mut term);
        assert!(
            pane.pending_resize.lock().is_none(),
            "gone once the snapshot shows it"
        );
        assert_eq!(pane.last_summary.lock().dimensions.cols, 100);
        drop(term);
        assert_eq!(pane.summary_without_waiting().terminal_size().cols, 100);
    }

    /// A resize that lands while the parser is between its last look at
    /// the note and letting go of the terminal is still applied by the
    /// parser, without another batch of output.
    #[test]
    fn a_resize_noted_during_the_parsers_last_moments_is_applied_by_the_parser() {
        let (pane, _sizes, _exit) = pane();
        let guard = pane.terminal.lock();
        // What `resize` does when both of its probes fail: the note alone.
        *pane.pending_resize.lock() = Some(size(70, 20));
        drop(guard);
        // The parser's batch ended; its trailing look finds the note.
        pane.perform_actions(vec![]);
        assert_eq!(pane.get_dimensions().cols, 70);
        assert!(pane.pending_resize.lock().is_none());
    }

    /// A split or spawn inherits the source pane's configuration; the
    /// read must not wait, and a change made while the parser holds the
    /// terminal reaches it from the parser.
    #[test]
    fn the_configuration_is_read_without_waiting_and_set_through_the_parser() {
        let (pane, _sizes, _exit) = pane();
        let guard = pane.terminal.lock();
        let started = Instant::now();
        let config = pane.get_config().expect("a local pane has a config");
        assert!(started.elapsed() < Duration::from_millis(50));
        let replacement: Arc<dyn TerminalConfiguration> = Arc::new(config::TermConfig::new());
        pane.set_config(Arc::clone(&replacement));
        assert!(
            Arc::ptr_eq(&pane.get_config().unwrap(), &replacement),
            "the pane answers with the new one at once"
        );
        assert!(
            !Arc::ptr_eq(&guard.get_config(), &replacement),
            "the terminal, held by the parser, has not"
        );
        drop(guard);
        pane.perform_actions(vec![]);
        assert!(
            Arc::ptr_eq(&pane.terminal.lock().get_config(), &replacement),
            "and the terminal took it with the next batch"
        );
    }

    /// Focusing a pane is what a person does right before typing into
    /// it; with the parser holding the terminal the focus change waits
    /// for the parser, not the other way round.
    #[test]
    fn a_focus_change_never_waits_for_the_parser() {
        let (pane, _sizes, _exit) = pane();
        let guard = pane.terminal.lock();
        let started = Instant::now();
        pane.focus_changed(true);
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(*pane.pending_focus.lock(), Some(true));
        let _ = pane.get_progress();
        drop(guard);
        pane.perform_actions(vec![]);
        assert!(
            pane.pending_focus.lock().is_none(),
            "the parser passed it on"
        );
    }

    #[test]
    fn a_resize_with_the_terminal_free_lands_at_once_and_refreshes_the_summary() {
        let (pane, _sizes, _exit) = pane();
        pane.resize(size(90, 20)).unwrap();
        assert_eq!(pane.get_dimensions().cols, 90);
        assert!(pane.pending_resize.lock().is_none());
        let guard = pane.terminal.lock();
        let summary = pane.summary_without_waiting();
        assert_eq!(summary.dimensions.cols, 90, "the stored copy moved with it");
        assert!(summary.resize_pending.is_none());
        drop(guard);
    }

    #[test]
    fn erasing_the_scrollback_refreshes_the_summary() {
        let (pane, _sizes, _exit) = pane();
        for _ in 0..40 {
            pane.perform_actions(vec![
                termwiz::escape::Action::Print('x'),
                termwiz::escape::Action::Control(termwiz::escape::ControlCode::LineFeed),
            ]);
        }
        let before = pane.get_dimensions().scrollback_rows;
        pane.erase_scrollback(ScrollbackEraseMode::ScrollbackAndViewport);
        let guard = pane.terminal.lock();
        let summary = pane.summary_without_waiting();
        assert!(summary.dimensions.scrollback_rows < before);
        assert_eq!(
            summary.dimensions.scrollback_rows,
            guard.screen().physical_rows
        );
        drop(guard);
    }

    /// A pty whose output is a pipe the test writes into.
    #[cfg(unix)]
    struct PipePty {
        read: filedescriptor::FileDescriptor,
    }

    #[cfg(unix)]
    impl MasterPty for PipePty {
        fn resize(&self, _size: PtySize) -> anyhow::Result<()> {
            Ok(())
        }
        fn get_size(&self) -> anyhow::Result<PtySize> {
            Ok(PtySize::default())
        }
        fn try_clone_reader(&self) -> anyhow::Result<Box<dyn std::io::Read + Send>> {
            Ok(Box::new(self.read.try_clone()?))
        }
        fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
            anyhow::bail!("no writer")
        }
        fn process_group_leader(&self) -> Option<libc::pid_t> {
            None
        }
        fn as_raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
            use std::os::unix::io::AsRawFd;
            Some(self.read.as_raw_fd())
        }
        fn tty_name(&self) -> Option<std::path::PathBuf> {
            None
        }
    }

    /// A pane with its reader running, and the end of the pipe it reads from.
    #[cfg(unix)]
    fn pane_on_a_pipe(
        id: PaneId,
    ) -> (
        Arc<dyn Pane>,
        filedescriptor::FileDescriptor,
        std::sync::mpsc::Sender<()>,
    ) {
        use std::os::unix::io::AsRawFd;
        ensure_scheduler();
        let pipe = filedescriptor::Pipe::new().unwrap();
        let raw = pipe.read.as_raw_fd();
        let (exit_tx, exit_rx) = std::sync::mpsc::channel();
        let terminal = Terminal::new(
            size(80, 24),
            Arc::new(config::TermConfig::new()),
            "ThinkTerm",
            "test",
            Box::new(Vec::new()),
        );
        let pane: Arc<dyn Pane> = Arc::new(LocalPane::new(
            id,
            terminal,
            Box::new(SleepingChild(exit_rx)),
            Box::new(PipePty { read: pipe.read }),
            Box::new(Vec::new()),
            0,
            "test".into(),
        ));
        let reader = pane.reader().unwrap().unwrap();
        let weak = Arc::downgrade(&pane);
        std::thread::spawn(move || crate::read_from_pane_pty(weak, None, reader, Some(raw)));
        (pane, pipe.write, exit_tx)
    }

    #[cfg(unix)]
    fn wait_for_title(pane: &Arc<dyn Pane>, want: &str) {
        let title = || pane.summary_without_waiting().title;
        let deadline = Instant::now() + Duration::from_secs(5);
        while title() != want && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(title(), want);
    }

    /// A reader going through a flood has no wait to be woken from, and stops
    /// for a pause all the same.
    #[cfg(unix)]
    #[test]
    fn a_reader_in_a_flood_stops_for_a_pause() {
        let (pane, mut program, _exit_tx) = pane_on_a_pipe(4244);
        program.write_all(b"\x1b]0;started\x07").unwrap();
        wait_for_title(&pane, "started");

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flood = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    program.write_all(&[b' '; 4096]).unwrap();
                }
                program
            }
        });
        std::thread::sleep(Duration::from_millis(20));

        let paused = crate::pause_pane_reader(4244, Duration::from_secs(2)).expect("pause");
        // The program stops only once the reader has gone on and made room.
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(20));
        drop(paused);
        let mut program = flood.join().unwrap();

        program.write_all(b"\x1b]0;after\x07").unwrap();
        wait_for_title(&pane, "after");
    }

    /// While a reader is paused, what the program writes stays in the pty;
    /// resuming applies it, and a kept pause never does.
    #[cfg(unix)]
    #[test]
    fn a_paused_reader_leaves_the_pty_output_where_it_is() {
        let (pane, mut program, _exit_tx) = pane_on_a_pipe(4242);
        let title = |pane: &Arc<dyn Pane>| pane.summary_without_waiting().title;

        program.write_all(b"\x1b]0;first\x07").unwrap();
        wait_for_title(&pane, "first");

        let paused = crate::pause_pane_reader(4242, Duration::from_secs(2)).expect("pause");
        program.write_all(b"\x1b]0;second\x07").unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(title(&pane), "first", "nothing is read while paused");
        drop(paused);
        wait_for_title(&pane, "second");

        let paused = crate::pause_pane_reader(4242, Duration::from_secs(2)).expect("pause again");
        program.write_all(b"\x1b]0;third\x07").unwrap();
        paused.keep();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(title(&pane), "second", "a kept pause never resumes");
    }

    /// What a busy pane's parser is asked to read runs when the batch it is
    /// on ends, once, on the thread that applied the batch, with the batch
    /// in it and the terminal there for the taking.
    #[test]
    fn a_read_left_for_the_parser_runs_after_its_batch() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (pane, _sizes, _exit) = pane();
        let batch = |text: &str| {
            let mut parser = termwiz::escape::parser::Parser::new();
            let mut actions = vec![];
            parser.parse(text.as_bytes(), |action| actions.push(action));
            pane.perform_actions(actions);
        };
        let parser_thread = std::thread::current().id();
        let runs = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&runs);
        pane.read_after_batch(Box::new(move |pane| {
            seen.fetch_add(1, Ordering::SeqCst);
            assert_eq!(std::thread::current().id(), parser_thread);
            assert!(!pane.render_state_is_contended(), "the terminal is free");
            let (_, lines) = pane.get_lines(0..1);
            assert!(lines[0].as_str().starts_with("first"), "the batch is in it");
        }))
        .ok()
        .expect("a local pane has a parser to ask");
        assert_eq!(runs.load(Ordering::SeqCst), 0, "no batch yet");

        batch("first");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "after the batch");
        batch(" second");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "and only once");
    }

    /// A resize that lands while a read has the terminal finds both of its
    /// probes failing and leaves its note, and with no output to follow
    /// nothing would come back for it.
    #[test]
    fn a_note_left_during_a_read_is_not_left_behind() {
        let (pane, _sizes, _exit) = pane();
        pane.read_after_batch(Box::new(|pane| {
            let pane = pane.downcast_ref::<LocalPane>().unwrap();
            *pane.pending_focus.lock() = Some(true);
        }))
        .ok()
        .expect("a local pane has a parser to ask");

        pane.perform_actions(vec![]);
        assert!(!pane.anything_pending(), "the batch saw to it");
    }

    /// A pane keeps these, so one that held its pane would never be freed.
    #[test]
    fn a_read_that_never_runs_goes_with_its_pane() {
        let (pane, _sizes, _exit) = pane();
        let held = Arc::new(());
        let kept = Arc::clone(&held);
        pane.read_after_batch(Box::new(move |_| drop(kept)))
            .ok()
            .expect("a local pane has a parser to ask");
        assert_eq!(Arc::strong_count(&held), 2, "waiting for a batch");
        drop(pane);
        assert_eq!(Arc::strong_count(&held), 1, "gone with the pane");
    }

    #[cfg(unix)]
    #[test]
    fn synchronized_output_survives_reader_pause_and_eof() {
        use std::os::unix::io::AsRawFd;
        use std::sync::atomic::Ordering;
        ensure_scheduler();
        let pipe = filedescriptor::Pipe::new().unwrap();
        let mut program = pipe.write;
        let raw = pipe.read.as_raw_fd();
        let (_exit_tx, exit_rx) = std::sync::mpsc::channel();
        let terminal = Terminal::new(
            size(80, 24),
            Arc::new(config::TermConfig::new()),
            "ThinkTerm",
            "test",
            Box::new(Vec::new()),
        );
        let pane: Arc<dyn Pane> = Arc::new(LocalPane::new(
            4243,
            terminal,
            Box::new(SleepingChild(exit_rx)),
            Box::new(PipePty { read: pipe.read }),
            Box::new(Vec::new()),
            0,
            "test".into(),
        ));
        let reader = pane.reader().unwrap().unwrap();
        let weak = Arc::downgrade(&pane);
        let reader_thread =
            std::thread::spawn(move || crate::read_from_pane_pty(weak, None, reader, Some(raw)));
        let title = |pane: &Arc<dyn Pane>| pane.summary_without_waiting().title;
        let wait_for = |want: &str| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while title(&pane) != want && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(title(&pane), want);
        };

        program.write_all(b"\x1b]0;initial\x07").unwrap();
        wait_for("initial");
        program.write_all(b"\x1b[?2026h").unwrap();
        program.write_all(&vec![b'x'; 256 * 1024 + 17]).unwrap();
        program.write_all(b"\x1b]0;held\x07").unwrap();
        let control = crate::PANE_READERS.lock().get(&4243).cloned().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !control.holding.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(control.holding.load(Ordering::SeqCst));
        let paused = crate::pause_pane_reader(4243, Duration::from_secs(2)).unwrap();
        assert_eq!(title(&pane), "initial");
        program.write_all(b"\x1b[?2026l").unwrap();
        drop(paused);
        wait_for("held");

        program.write_all(b"\x1b[?2026h\x1b]0;final\x07").unwrap();
        drop(program);
        wait_for("final");
        reader_thread.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while control.pending_bytes() != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(control.pending_bytes(), 0);
    }
}
