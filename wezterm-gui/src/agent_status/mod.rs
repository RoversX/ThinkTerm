//! GUI facade over the agent-status core.
//!
//! Detection itself lives in `mux::agent_status` and runs in whichever
//! process *owns* a pane; this module only presents the results (panel
//! snapshots, settings-page metadata) and hosts the pieces that are
//! genuinely GUI concerns (the feature toggle, the login-shell PATH
//! probe, display names).
//!
//! Deliberately NOT re-exported: `engine::*`, `DetectionInput`,
//! `contract::parse`, `identify::*`. Only `mux::agent_status::reload_rules`
//! crosses the boundary (the panel's reload button). That makes GUI-side
//! double detection structurally impossible — a pane's status has exactly
//! one producer, the mux that owns it. Do not widen this surface.

pub(crate) use thinkterm_proto::{AgentEvidence, AgentState};

use crate::workspace_threads::WorkspaceThreadWorkStatus;
use mux::pane::{Pane, PaneId};
use mux::Mux;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Clickable controls in the Agents panel; carried by
/// `UIItemType::RightSidebarAgent`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentPanelAction {
    ReloadRules,
    /// Focus the pane running this agent (it lives in this window).
    Reveal(PaneId),
    /// The pane lives in another window: clicking deep-focuses its tab
    /// and pane, then activates the backing thread (workspace switch,
    /// window brought forward) via the left sidebar's activation path.
    RevealElsewhere(PaneId),
}

/// Snapshot of one agent pane for the Agents panel.
#[derive(Clone, Debug)]
pub(crate) struct AgentPaneStatus {
    pub pane_id: PaneId,
    /// Manifest/contract agent id, e.g. "claude".
    pub agent_id: String,
    pub state: AgentState,
    #[allow(dead_code)]
    pub evidence: AgentEvidence,
    /// Kept for the upcoming session-resume integration (spawn spec).
    #[allow(dead_code)]
    pub session_id: Option<String>,
    /// Pane title with any leading progress marker stripped.
    pub title: String,
    pub workspace: String,
    /// Mux window hosting the pane, resolved once per snapshot.
    pub window_id: Option<mux::window::WindowId>,
    /// Human place name ("Project · Thread"), falling back to the raw
    /// workspace string. Resolved once per snapshot: the paint loop must
    /// not walk the thread store per row per frame.
    pub place: String,
    /// Unix seconds when `state` last changed, on the detecting host's
    /// clock (approximate across hosts).
    #[allow(dead_code)]
    pub since_unix: u64,
}

/// One-line result of the last panel action, painted under the toolbar.
/// Expires on its own: the message is an acknowledgement, not a pinned
/// banner, and it is process-global (every window shows it).
static PANEL_STATUS: Mutex<Option<(String, Instant)>> = Mutex::new(None);
const PANEL_STATUS_TTL: Duration = Duration::from_secs(6);

pub(crate) fn panel_status() -> Option<String> {
    let guard = PANEL_STATUS.lock().ok()?;
    let (message, at) = guard.as_ref()?;
    (at.elapsed() < PANEL_STATUS_TTL).then(|| message.clone())
}

pub(crate) fn set_panel_status(message: String) {
    if let Ok(mut slot) = PANEL_STATUS.lock() {
        *slot = Some((message, Instant::now()));
    }
}

pub(crate) fn enabled() -> bool {
    // Both gates: without the Lua master switch the detector never runs,
    // and a permanently-empty panel would just look broken.
    crate::native_settings::agent_panel_enabled()
        && config::configuration().agent_status_detection
}

/// Per-pane work status for the sidebar thread scan. `None` when the pane
/// runs no known agent — callers fall back to the native title/progress
/// signals, so the flag-off path stays identical to pre-agent behavior.
pub(crate) fn agent_work_status(pane: &dyn Pane) -> Option<WorkspaceThreadWorkStatus> {
    let status = pane.agent_status()?;
    // Three cases where the agent status says nothing about the pane's
    // work, and pinning Idle would mask the native title/progress signals:
    // an ended session, an Unknown verdict, and the Fallback tier — the
    // latter means "recognized agent, but neither screen nor contract
    // spoke", and the pane's own OSC 9;4 / title spinner may know better.
    if status.ended
        || status.state == AgentState::Unknown
        || status.evidence == AgentEvidence::Fallback
    {
        return None;
    }
    Some(match status.state {
        AgentState::Working => WorkspaceThreadWorkStatus::Running,
        AgentState::Blocked => WorkspaceThreadWorkStatus::NeedsAttention,
        AgentState::Idle | AgentState::Unknown => WorkspaceThreadWorkStatus::Idle,
    })
}

/// While an agent spins, the panel repaints at animation rate; walking
/// every window's every tab per frame is wasted work when nothing
/// changed. Status changes call [`invalidate_agent_pane_cache`], so the
/// TTL only bounds staleness of titles/workspaces.
static AGENT_PANES_CACHE: Mutex<Option<(Instant, Vec<AgentPaneStatus>)>> = Mutex::new(None);
const AGENT_PANES_TTL: Duration = Duration::from_millis(200);

/// Called from the AgentStatusChanged repaint path so a state change is
/// visible on the very next paint rather than a TTL later.
pub(crate) fn invalidate_agent_pane_cache() {
    if let Ok(mut slot) = AGENT_PANES_CACHE.lock() {
        *slot = None;
    }
}

/// Snapshot for the Agents panel: every pane in the mux with a known
/// agent status — locally detected or mirrored from a remote server —
/// minus sessions that reported their own end.
pub(crate) fn list_agent_panes() -> Vec<AgentPaneStatus> {
    if let Ok(guard) = AGENT_PANES_CACHE.lock() {
        if let Some((at, cached)) = guard.as_ref() {
            if at.elapsed() < AGENT_PANES_TTL {
                return cached.clone();
            }
        }
    }
    let mux = Mux::get();
    // One pass over the window/tab topology instead of a full
    // `resolve_pane_id` scan per agent pane.
    let mut workspace_by_pane: HashMap<PaneId, (mux::window::WindowId, String)> = HashMap::new();
    for window_id in mux.iter_windows() {
        let Some(window) = mux.get_window(window_id) else {
            continue;
        };
        let workspace = window.get_workspace().to_string();
        for tab in window.iter() {
            for pane in tab.iter_all_panes() {
                workspace_by_pane.insert(pane.pane_id(), (window_id, workspace.clone()));
            }
        }
    }
    let panes: Vec<AgentPaneStatus> = mux
        .iter_panes()
        .into_iter()
        .filter_map(|pane| {
            let status = pane.agent_status()?;
            if status.ended {
                return None;
            }
            let (window_id, workspace) = match workspace_by_pane.get(&pane.pane_id()) {
                Some((window_id, workspace)) => (Some(*window_id), workspace.clone()),
                None => (None, String::new()),
            };
            let place = if workspace.is_empty() {
                String::new()
            } else {
                crate::workspace_threads::thread_display_name_for_workspace(&workspace)
                    .unwrap_or_else(|| workspace.clone())
            };
            let raw_title = pane.get_title();
            let title =
                crate::termwindow::ui::status_icon::split_leading_legacy_progress_marker(
                    &raw_title,
                )
                .unwrap_or(&raw_title)
                .to_string();
            Some(AgentPaneStatus {
                pane_id: pane.pane_id(),
                agent_id: status.agent_id,
                state: status.state,
                evidence: status.evidence,
                session_id: status.session_id,
                title,
                workspace,
                window_id,
                place,
                since_unix: status.since_unix,
            })
        })
        .collect();
    if let Ok(mut slot) = AGENT_PANES_CACHE.lock() {
        *slot = Some((Instant::now(), panes.clone()));
    }
    panes
}

pub(crate) fn display_name(agent_id: &str) -> String {
    match agent_id {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "copilot" => "Copilot CLI",
        "cursor" => "Cursor Agent",
        "pi" => "Pi",
        "opencode" => "OpenCode",
        "kimi" => "Kimi Code",
        "omp" => "OMP",
        "soul" => "Soul",
        // A custom manifest or contract id has no curated name; the raw
        // id (title-cased) beats an indistinguishable generic "Agent".
        other => {
            let mut chars = other.chars();
            return match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => "Agent".to_string(),
            };
        }
    }
    .to_string()
}

/// How each supported agent is covered, for the settings Integrations list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntegrationKind {
    /// Covered by bundled screen-rule manifests; nothing to install.
    ScreenRules,
    /// Speaks the native `THINKTERM_AGENT` protocol itself.
    Native,
    /// Deeper integration planned; no screen rules yet.
    Pending,
}

/// The supported agents shown in Settings → Agents → Integrations, in
/// display order: (agent id, PATH probe names, coverage).
pub(crate) const SUPPORTED_AGENTS: &[(&str, &[&str], IntegrationKind)] = &[
    ("claude", &["claude"], IntegrationKind::ScreenRules),
    ("codex", &["codex"], IntegrationKind::ScreenRules),
    ("copilot", &["copilot"], IntegrationKind::ScreenRules),
    ("cursor", &["cursor-agent", "cursor"], IntegrationKind::ScreenRules),
    ("pi", &["pi"], IntegrationKind::ScreenRules),
    ("opencode", &["opencode"], IntegrationKind::ScreenRules),
    ("kimi", &["kimi"], IntegrationKind::ScreenRules),
    ("omp", &["omp"], IntegrationKind::Pending),
    ("soul", &["soul"], IntegrationKind::Native),
];

static PATH_PROBE: Mutex<Option<HashMap<&'static str, bool>>> = Mutex::new(None);

/// Directories searched when probing for agent executables. A GUI process
/// launched from Finder/Dock inherits launchd's minimal PATH (no
/// /opt/homebrew/bin, no ~/.local/bin), so the login shell's PATH is
/// merged in once per process, plus a few well-known install locations
/// as insurance for shells that only extend PATH interactively.
fn probe_dirs() -> &'static Vec<std::path::PathBuf> {
    use std::sync::OnceLock;
    static DIRS: OnceLock<Vec<std::path::PathBuf>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut dirs: Vec<std::path::PathBuf> = Vec::new();
        let push_all = |value: &std::ffi::OsStr, dirs: &mut Vec<std::path::PathBuf>| {
            for dir in std::env::split_paths(value) {
                if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
                    dirs.push(dir);
                }
            }
        };
        if let Some(path) = std::env::var_os("PATH") {
            push_all(&path, &mut dirs);
        }
        #[cfg(unix)]
        {
            // Not `$SHELL`: env-bootstrap unconditionally removes it at
            // startup (stale after `chsh`), so reading it here would run
            // `/bin/sh -l` forever and never see the PATH set up by the
            // user's real shell rc (mise/asdf/nvm installs). get_shell()
            // resolves from the password database instead.
            let shell = portable_pty::CommandBuilder::new_default_prog().get_shell();
            if let Some(path) = login_shell_path(&shell) {
                push_all(std::ffi::OsStr::new(path.trim()), &mut dirs);
            }
            for fallback in ["/opt/homebrew/bin", "/usr/local/bin"] {
                let dir = std::path::PathBuf::from(fallback);
                if !dirs.contains(&dir) {
                    dirs.push(dir);
                }
            }
            let local_bin = config::HOME_DIR.join(".local").join("bin");
            if !dirs.contains(&local_bin) {
                dirs.push(local_bin);
            }
        }
        dirs
    })
}

/// The login shell's `$PATH`, bounded in time. Shell profiles can block
/// forever on interactive steps (`read`, a gpg pinentry, a keychain
/// unlock); an unbounded wait here would wedge the probe worker with the
/// `PATH_PROBE_RUNNING` latch held and hang every later `probe_dirs`
/// caller on the OnceLock, so a hung shell is killed and the fallback
/// dirs stand in.
#[cfg(unix)]
fn login_shell_path(shell: &str) -> Option<String> {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    const BUDGET: Duration = Duration::from_secs(5);

    let mut child = std::process::Command::new(shell)
        .args(["-l", "-c", "printf %s \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read on a helper thread: a profile can spawn a background process
    // that inherits the pipe's write end, in which case a post-exit read
    // would block even though the shell itself is done.
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut stdout) = stdout {
            use std::io::Read;
            let _ = stdout.read_to_string(&mut buf);
        }
        let _ = tx.send(buf);
    });
    let deadline = Instant::now() + BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                // Bounded even here, for the inherited-write-end case;
                // losing the answer beats blocking the probe worker.
                return rx.recv_timeout(Duration::from_millis(500)).ok();
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    log::warn!(
                        "login shell {shell} did not produce $PATH within {BUDGET:?} \
                         (interactive profile step?); using fallback probe dirs"
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// A PATH hit must be an executable *file*: a stray non-executable file
/// with an agent's name must not report the agent as installed.
fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

/// Whether this agent's executable is reachable on the probe path. A pure
/// cache read: painting must never run the login shell or stat files.
/// Answers `false` until `refresh_path_probe`'s worker has reported.
pub(crate) fn agent_on_path(agent_id: &str) -> bool {
    PATH_PROBE
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(|map| map.get(agent_id).copied()))
        .unwrap_or(false)
}

static PATH_PROBE_RUNNING: AtomicBool = AtomicBool::new(false);

/// Re-run the executable probe on a worker thread (the login-shell PATH
/// merge can take seconds under a heavy shell profile, and must never
/// block the UI thread). Repaints all windows when the result lands.
pub(crate) fn refresh_path_probe() {
    if PATH_PROBE_RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(|| {
        // The latch must clear even if the probe panics (a stuck `true`
        // would silently freeze the Integrations list for the process
        // lifetime), so it lives in a drop guard.
        struct ProbeLatch;
        impl Drop for ProbeLatch {
            fn drop(&mut self) {
                PATH_PROBE_RUNNING.store(false, Ordering::Release);
            }
        }
        let _latch = ProbeLatch;
        let dirs = probe_dirs();
        let mut map = HashMap::new();
        for (id, names, _) in SUPPORTED_AGENTS {
            let found = dirs.iter().any(|dir| {
                names.iter().any(|name| {
                    let file = if cfg!(windows) {
                        format!("{name}.exe")
                    } else {
                        (*name).to_string()
                    };
                    is_executable_file(&dir.join(file))
                })
            });
            map.insert(*id, found);
        }
        if let Ok(mut guard) = PATH_PROBE.lock() {
            *guard = Some(map);
        }
        promise::spawn::spawn_into_main_thread(async {
            // The settings window is the surface that reads this result,
            // and it is not in the frontend's known-windows list — it
            // must be invalidated explicitly.
            crate::settings_window::invalidate_open_settings_window();
            if let Some(front_end) = crate::frontend::try_front_end() {
                front_end.invalidate_all_windows();
            }
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use super::display_name;

    #[test]
    fn display_name_falls_back_to_the_raw_id() {
        assert_eq!(display_name("claude"), "Claude Code");
        assert_eq!(display_name("myagent"), "Myagent");
        assert_eq!(display_name(""), "Agent");
    }
}
