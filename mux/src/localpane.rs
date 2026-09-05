use crate::domain::DomainId;
use crate::pane::{
    CachePolicy, CloseReason, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, PaneSummary,
    Pattern, SearchResult, WithPaneLines,
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
    pty: Mutex<Box<dyn MasterPty>>,
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
                    let success = match status.success() {
                        true => true,
                        false => configuration()
                            .clean_exit_codes
                            .contains(&status.exit_code()),
                    };

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
        self.terminal.lock().set_config(config);
    }

    fn get_config(&self) -> Option<Arc<dyn TerminalConfiguration>> {
        Some(self.terminal.lock().get_config())
    }

    fn perform_actions(&self, mut actions: Vec<termwiz::escape::Action>) {
        if crate::has_external_kitty_image_data_source(&actions) {
            let kitty_graphics_enabled =
                { self.terminal.lock().get_config().enable_kitty_graphics() };
            if kitty_graphics_enabled {
                crate::materialize_kitty_image_data_sources(&mut actions);
            }
        }
        let mut term = self.terminal.lock();
        self.apply_pending_resize(&mut term);
        term.perform_actions(actions);
        self.apply_pending_resize(&mut term);
        self.store_summary(&mut term);
        drop(term);
        // A resize that arrived after that last look, while the terminal
        // was still held here, found both of its own probes failing and
        // left its note; nothing else would read it until the next batch,
        // which a quiet program may never write. Looked at again with the
        // terminal free, for as long as notes keep arriving: one left
        // after the last look is applied by the resize's own second
        // probe, which finds the terminal free.
        while self.pending_resize.lock().is_some() {
            let mut term = self.terminal.lock();
            self.apply_pending_resize(&mut term);
            self.store_summary(&mut term);
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

    fn send_paste(&self, text: &str) -> Result<(), Error> {
        Mux::get().record_input_for_current_identity();
        if self.tmux_domain.lock().is_some() {
            Ok(())
        } else {
            self.terminal.lock().send_paste(text)
        }
    }

    fn get_title(&self) -> String {
        let mut title = self.terminal.lock().get_title().to_string();
        self.spice_title(&mut title);
        title
    }

    fn get_progress(&self) -> Progress {
        self.terminal.lock().get_progress()
    }

    fn agent_osc_evidence(&self) -> wezterm_term::AgentOscEvidence {
        self.terminal.lock().agent_osc_evidence()
    }

    fn clear_agent_osc_evidence(&self) {
        self.terminal.lock().clear_agent_osc_state();
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
        self.terminal.lock().focus_changed(focused);
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
/// This block schedules a background thread to wait for the child
/// to terminate, and then nudge the muxer to check for dead processes.
/// Without this, typing `exit` in `cmd.exe` would keep the pane around
/// until something else triggered the mux to prune dead processes.
fn split_child(
    mut process: Box<dyn Child>,
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
        tx.try_send(status).ok();
        promise::spawn::spawn_into_main_thread(async move {
            let mux = Mux::get();
            mux.prune_dead_windows();
        })
        .detach();
    });

    (rx, signaller, pid)
}

impl LocalPane {
    pub fn new(
        pane_id: PaneId,
        mut terminal: Terminal,
        process: Box<dyn Child + Send>,
        pty: Box<dyn MasterPty>,
        writer: Box<dyn Write + Send>,
        domain_id: DomainId,
        command_description: String,
    ) -> Self {
        let (process, signaller, pid) = split_child(process);

        terminal.set_device_control_handler(Box::new(LocalPaneDCSHandler {
            pane_id,
            tmux_domain: None,
        }));
        terminal.set_notification_handler(Box::new(LocalPaneNotifHandler { pane_id }));
        let last_summary = Mutex::new(PaneSummary::from_terminal(&mut terminal, false));

        Self {
            pane_id,
            terminal: Mutex::new(terminal),
            process: Mutex::new(ProcessState::Running {
                child_waiter: process,
                pid,
                signaller,
                killed: false,
            }),
            pty: Mutex::new(pty),
            writer: Mutex::new(writer),
            domain_id,
            tmux_domain: Mutex::new(None),
            proc_list: Mutex::new(None),
            #[cfg(unix)]
            leader: Arc::new(Mutex::new(None)),
            command_description,
            last_summary,
            pending_resize: Mutex::new(None),
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
        // The note is taken and its lock let go before the resize: a
        // resize reflows the scrollback, and `summary_without_waiting`
        // reads the note on the thread that must not wait.
        let pending = self.pending_resize.lock().take();
        if let Some(size) = pending {
            term.resize(size);
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
mod summary_tests {
    use super::*;
    use portable_pty::PtySize;
    use std::sync::Mutex as StdMutex;

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

    /// A pty that only remembers the sizes it was given.
    struct RecordingPty(Arc<StdMutex<Vec<PtySize>>>);

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
            None
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

    fn pane() -> (
        LocalPane,
        Arc<StdMutex<Vec<PtySize>>>,
        std::sync::mpsc::Sender<()>,
    ) {
        // The waiter thread `LocalPane::new` starts hops to the main
        // thread when the child exits; with no scheduler that is a panic
        // that poisons promise's scheduler lock for every test after.
        static SCHEDULER: std::sync::Once = std::sync::Once::new();
        SCHEDULER.call_once(|| {
            promise::spawn::SimpleExecutor::new();
        });
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
            Box::new(RecordingPty(Arc::clone(&sizes))),
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
}
