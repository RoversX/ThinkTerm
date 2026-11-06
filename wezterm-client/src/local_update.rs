//! Bringing the session server of this machine to the client's build.
//!
//! The GUI keeps its local terminals in a `thinkterm-mux-server` (see
//! `UnixDomain::local_session_host`). After the application is updated the
//! next launch finds a server of the previous build still running with
//! every session in it. Rather than refuse it (codec mismatch) or serve
//! stale behaviour (same codec, older build), the client runs the new
//! server binary with `--takeover`: the running one hands its panes over
//! and exits, and the client reconnects to the same runtime id.

use anyhow::Context as _;
use config::UnixDomain;
use mux::connui::ConnectionUI;
use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How often, and for how long, a takeover is tried before the running
/// server is kept (here and on a remote host after an update). One that
/// fails before the successor owns the panes leaves that server as it was,
/// and the failure need not recur: on macOS a server built before the
/// handoff learned to wait for room in its stream gives up on a pane whose
/// message left the stream all but full -- though a pane whose message
/// comes out the same each time can fail every time. A failure that came
/// slowly, a step timing out, would only be as slow again.
pub(crate) const TAKEOVER_ATTEMPTS: u32 = 3;
pub(crate) const TAKEOVER_RETRY_WINDOW: Duration = Duration::from_secs(30);
pub(crate) const TAKEOVER_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Whether a takeover that failed with exit status `code` is tried again,
/// `elapsed` after the first one started. One that could not start at all
/// would meet the same again.
pub(crate) fn worth_retrying(code: Option<i32>, attempts_made: u32, elapsed: Duration) -> bool {
    code != Some(mux::session_server::TAKEOVER_CANNOT_HAND_OVER)
        && attempts_made < TAKEOVER_ATTEMPTS
        && elapsed < TAKEOVER_RETRY_WINDOW
}

/// The pid file that names the server on `unix`'s socket, when it does: the
/// pid file is one per profile, the socket path is per domain, so a domain
/// on a custom path may be another server's.
fn pid_file_for(unix: &UnixDomain) -> Option<std::path::PathBuf> {
    (unix.socket_path() == config::RUNTIME_DIR.join(config::runtime_file_name("sock")))
        .then(|| config::configuration().daemon_options.pid_file())
}

/// Whether the server holding the pid file is no longer the one that held
/// it before the takeover: another client's successor took it over, or it
/// is gone. Unknown before, it cannot be told.
fn replaced(before: Option<u32>, now: Option<u32>) -> bool {
    before.is_some() && now != before
}

/// Whether this process started (or took over) the local session server,
/// as opposed to finding one already running. The GUI only ever stops a
/// server it started itself.
static STARTED_LOCAL_SESSION_HOST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn note_local_session_host_started() {
    STARTED_LOCAL_SESSION_HOST.store(true, std::sync::atomic::Ordering::Release);
}

pub fn local_session_host_started_by_us() -> bool {
    STARTED_LOCAL_SESSION_HOST.load(std::sync::atomic::Ordering::Acquire)
}

/// Whether the server running here, of `version`, is a newer release than
/// this client: an app left behind by an update of the server. That server
/// stays, sessions and all. A takeover would at best be refused, and the
/// stop that follows one on Windows would end its terminals.
pub fn server_is_newer(version: &str) -> bool {
    thinkterm_update::is_older_release(config::wezterm_version(), version)
}

/// Run the server this client ships with as a successor of the one
/// listening on `unix`'s socket. `Ok(true)` when that one no longer serves
/// -- this build's successor does, or another client's that took it over
/// between tries -- and the caller reconnects; `Ok(false)` when the running
/// server refused or the takeover failed every try (its output was relayed
/// to the UI); `Err` when the command could not be run at all.
pub fn take_over_local_server(unix: &UnixDomain, ui: &ConnectionUI) -> anyhow::Result<bool> {
    let argv = takeover_argv(unix)?;
    ui.output_str(&format!(
        "Handing the sessions of the running mux server to this build ({})...\n",
        config::wezterm_version()
    ));
    let pid_file = pid_file_for(unix);
    let holder = || pid_file.as_deref().and_then(mux::session_server::pid_holding);
    let running = holder();
    let started = Instant::now();
    let mut attempts = 0;
    loop {
        attempts += 1;
        let status = run_takeover(&argv, ui)?;
        if status.success() {
            note_local_session_host_started();
            ui.output_str("Handed over; reconnecting.\n");
            return Ok(true);
        }
        let again = worth_retrying(status.code(), attempts, started.elapsed());
        if again {
            std::thread::sleep(TAKEOVER_RETRY_DELAY);
        }
        // Another client's successor took it over meanwhile: that one
        // serves, and taking it over would only hand the same sessions over
        // once more.
        if replaced(running, holder()) {
            ui.output_str(&format!(
                "The running server could not hand over ({status}) and has been replaced \
                 since; reconnecting.\n"
            ));
            return Ok(true);
        }
        if !again {
            ui.output_str(&format!(
                "The running server could not hand over ({status}); keeping it.\n"
            ));
            return Ok(false);
        }
        ui.output_str(&format!(
            "The running server could not hand over ({status}); trying again ({} of \
             {TAKEOVER_ATTEMPTS})...\n",
            attempts + 1
        ));
    }
}

/// Run the takeover command once and wait for how it went.
fn run_takeover(argv: &[std::ffi::OsString], ui: &ConnectionUI) -> anyhow::Result<ExitStatus> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The GUI runs with a locked-down umask; the server and every shell it
    // spawns must get the one the user had (as unix_connect does).
    #[cfg(unix)]
    if let Some(mask) = umask::UmaskSaver::saved_umask() {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(move || {
                libc::umask(mask);
                Ok(())
            });
        }
    }
    // A console program started from the GUI would get a window of its own.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("running {:?}", argv))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let relay = |ui: ConnectionUI, mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(len) = pipe.read(&mut buf) {
                if len == 0 {
                    break;
                }
                ui.output_str(&String::from_utf8_lossy(&buf[..len]));
            }
        })
    };
    let out_thread = stdout.map(|pipe| relay(ui.clone(), Box::new(pipe)));
    let err_thread = stderr.map(|pipe| relay(ui.clone(), Box::new(pipe)));
    // A daemonizing takeover reports within seconds; a serve command that
    // does not daemonize would never return, so wait with a limit.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for the takeover")? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("the takeover did not report within 120 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    if let Some(thread) = out_thread {
        let _ = thread.join();
    }
    if let Some(thread) = err_thread {
        let _ = thread.join();
    }
    Ok(status)
}

/// The serve command of the domain (`thinkterm-mux-server --daemonize` next
/// to this executable unless configured otherwise) with `--takeover`
/// appended, so a custom serve command (`wsl -e ...`) takes over the same
/// way it starts.
fn takeover_argv(unix: &UnixDomain) -> anyhow::Result<Vec<std::ffi::OsString>> {
    let mut argv = unix.serve_command()?;
    if argv.is_empty() {
        anyhow::bail!("the serve command of domain {} is empty", unix.name);
    }
    argv.push("--takeover".into());
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::{
        replaced, takeover_argv, worth_retrying, TAKEOVER_ATTEMPTS, TAKEOVER_RETRY_WINDOW,
    };
    use config::UnixDomain;
    use std::time::Duration;

    #[test]
    fn a_takeover_that_failed_quickly_is_tried_again_up_to_the_limit() {
        let quick = Duration::from_secs(1);
        let failed = Some(1);
        assert!(worth_retrying(failed, 1, quick));
        assert!(worth_retrying(failed, TAKEOVER_ATTEMPTS - 1, quick));
        assert!(!worth_retrying(failed, TAKEOVER_ATTEMPTS, quick));
        // A step that timed out would time out again.
        assert!(!worth_retrying(failed, 1, TAKEOVER_RETRY_WINDOW));
        // One that could not start at all would not start the next time.
        let cannot = Some(mux::session_server::TAKEOVER_CANNOT_HAND_OVER);
        assert!(!worth_retrying(cannot, 1, quick));
    }

    #[test]
    fn a_server_replaced_between_tries_is_not_taken_over_again() {
        assert!(!replaced(Some(500), Some(500)));
        // Another client's successor holds the pid file now.
        assert!(replaced(Some(500), Some(900)));
        // Nothing holds it: the server is gone.
        assert!(replaced(Some(500), None));
        // No pid file held before: there is nothing to compare.
        assert!(!replaced(None, Some(900)));
        assert!(!replaced(None, None));
    }

    #[test]
    fn the_takeover_is_the_serve_command_plus_the_flag() {
        let unix = UnixDomain {
            name: "unix".into(),
            serve_command: Some(vec!["wsl".into(), "-e".into(), "thinkterm-mux-server".into(), "--daemonize".into()]),
            ..Default::default()
        };
        let argv = takeover_argv(&unix).unwrap();
        assert_eq!(
            argv,
            vec!["wsl", "-e", "thinkterm-mux-server", "--daemonize", "--takeover"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_default_serve_command_is_the_bundled_server() {
        let argv = takeover_argv(&UnixDomain::default()).unwrap();
        let exe = std::path::Path::new(&argv[0]);
        // The stem, so the `.exe` a Windows build carries does not count.
        assert_eq!(
            exe.file_stem().and_then(|stem| stem.to_str()),
            Some("thinkterm-mux-server"),
            "{:?}",
            exe
        );
        assert_eq!(argv[1], "--daemonize");
        assert_eq!(argv[2], "--takeover");
    }
}
