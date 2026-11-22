//! Keeping the machine's node running: what the mux server does. The node
//! itself runs in `thinkterm-tailnet`, a program beside the server's own (see
//! the crate's docs). The server starts it while the settings turn the
//! tailnet on, and again when it dies; it ends by itself, with the server
//! that started it or once the settings turn the tailnet off.
//!
//! Tied to the server that started it rather than kept across a takeover:
//! macOS holds the server responsible, in privacy terms, for what it starts
//! -- reaching the local network included -- and a program outliving its
//! server would answer to one that is gone (see the server's
//! `exec_as_own_tcc_identity`).

use crate::settings;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The program, beside the server's executable.
const PROGRAM: &str = "thinkterm-tailnet";

/// How often the program, and whether the settings file changed, are
/// looked at while the tailnet is on or the program runs; and while neither,
/// as the settings were looked at before the node had a program of its own.
const POLL: Duration = Duration::from_secs(1);
const IDLE_POLL: Duration = Duration::from_secs(3);

/// How long the program waits to be started again after it ended while the
/// tailnet was on, at first and at most: the wait doubles each time it ends
/// within RAN_LONG_ENOUGH of its start.
const RESTART_FIRST: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_secs(60);
const RAN_LONG_ENOUGH: Duration = Duration::from_secs(60);

/// How long a connection asked for during a takeover waits for the
/// predecessor's node to have gone.
const TAKEOVER_WAIT: Duration = Duration::from_secs(15);

/// How long after the predecessor has exited its `thinkterm-tailnet` may
/// take to end with it: it looks every second (see thinkterm-tailnet-node).
const PREDECESSOR_ENDS: Duration = Duration::from_secs(2);

/// Set from `will_hold` until the predecessor's node has had time to go,
/// PREDECESSOR_ENDS after this server set out to keep its own, or the
/// tailnet is off.
static TAKING_OVER: AtomicBool = AtomicBool::new(false);

static KEEPING: OnceLock<()> = OnceLock::new();

/// This server takes over from a predecessor, whose `thinkterm-tailnet`
/// holds the node until the predecessor exits, and calls [`keep_node`]
/// after that: connections to the tailnet's machines meanwhile wait for the
/// predecessor's node to go (see `wait_out_takeover`), rather than go
/// through it and end with it. After that, they go through whichever
/// process holds the node: this server's own, or another one's.
pub fn will_hold() {
    TAKING_OVER.store(true, Ordering::SeqCst);
}

/// Keeps `thinkterm-tailnet` running while the settings turn the tailnet on,
/// from a thread of its own. Calling it again does nothing.
pub fn keep_node() {
    if KEEPING.set(()).is_err() {
        return;
    }
    let program = match program() {
        Ok(program) => program,
        Err(err) => {
            log::error!("tailnet: finding {PROGRAM}: {err}");
            TAKING_OVER.store(false, Ordering::SeqCst);
            return;
        }
    };
    let path = match settings::path() {
        Ok(path) => path,
        Err(err) => {
            log::error!("tailnet: {err:#}");
            TAKING_OVER.store(false, Ordering::SeqCst);
            return;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("tailnet-keeper".into())
        .spawn(move || {
            let mut keeper = Keeper::new(program);
            let started = Instant::now();
            // Read again only once the file has changed.
            let mut seen = None;
            let mut enabled = false;
            loop {
                let stamp = settings::stamp(&path);
                if seen != Some(stamp) {
                    seen = Some(stamp);
                    enabled = settings::load_summary().is_ok_and(|settings| settings.enabled);
                }
                keeper.step(enabled, Instant::now());
                if !enabled || started.elapsed() >= PREDECESSOR_ENDS {
                    TAKING_OVER.store(false, Ordering::SeqCst);
                }
                let running = keeper.child.is_some();
                std::thread::sleep(if enabled || running { POLL } else { IDLE_POLL });
            }
        });
    if let Err(err) = spawned {
        log::error!("tailnet: starting the thread that keeps {PROGRAM} running: {err}");
        TAKING_OVER.store(false, Ordering::SeqCst);
    }
}

/// While this server takes over (see [`will_hold`]), waits for the
/// predecessor's node to have gone, for up to TAKEOVER_WAIT of `timeout`.
/// False when the session is closed meanwhile, or the wait runs out: the
/// connection fails rather than go through the predecessor's node.
pub(crate) fn wait_out_takeover(timeout: Duration, cancelled: &dyn Fn() -> bool) -> bool {
    wait_while(&TAKING_OVER, timeout.min(TAKEOVER_WAIT), cancelled)
}

/// Waits while `flag` is set, for up to `wait`: whether it was let go of.
fn wait_while(flag: &AtomicBool, wait: Duration, cancelled: &dyn Fn() -> bool) -> bool {
    let deadline = Instant::now() + wait;
    while flag.load(Ordering::SeqCst) {
        if cancelled() {
            return false;
        }
        if Instant::now() >= deadline {
            log::warn!("tailnet: the predecessor's node did not go in time");
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

/// The program beside the server's executable: beside the file itself, not
/// a link to it, as the command-line tools are linked into a bin directory
/// from the app bundle. On Windows, where the programs are installed side
/// by side and linked nowhere, the path stays as it is: its canonical form
/// there is a `\\?\` one.
fn program() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    #[cfg(unix)]
    let exe = exe.canonicalize().unwrap_or(exe);
    Ok(exe.with_file_name(format!("{PROGRAM}{}", std::env::consts::EXE_SUFFIX)))
}

/// The program as this server runs it.
struct Keeper {
    program: PathBuf,
    /// The program while it runs, and when it was started.
    child: Option<(Child, Instant)>,
    /// When it may be started next.
    next_start: Instant,
    /// The wait before the next start, should it end soon again.
    wait: Duration,
}

impl Keeper {
    fn new(program: PathBuf) -> Self {
        Self {
            program,
            child: None,
            next_start: Instant::now(),
            wait: RESTART_FIRST,
        }
    }

    /// Reaps the program once it has ended, and starts it while the tailnet
    /// is `enabled`, it is not running and its wait is over.
    fn step(&mut self, enabled: bool, now: Instant) {
        if let Some((child, started)) = &mut self.child {
            match child.try_wait() {
                Ok(None) => {}
                Ok(Some(status)) => {
                    let ran = now.saturating_duration_since(*started);
                    self.child = None;
                    if enabled {
                        if ran >= RAN_LONG_ENOUGH {
                            self.wait = RESTART_FIRST;
                        }
                        log::warn!(
                            "tailnet: {PROGRAM} ended ({status}) after {ran:?}; \
                             starting it again in {:?}",
                            self.wait
                        );
                        self.wait_before_next(now);
                    } else {
                        // It ended as the tailnet was turned off: on again,
                        // it starts at once.
                        log::info!("tailnet: {PROGRAM} ended ({status})");
                        self.wait = RESTART_FIRST;
                        self.next_start = now;
                    }
                }
                Err(err) => log::warn!("tailnet: waiting for {PROGRAM}: {err}"),
            }
        }
        if !enabled || self.child.is_some() || now < self.next_start {
            return;
        }
        match start(&self.program) {
            Ok(child) => {
                log::info!("tailnet: started {PROGRAM}, pid {}", child.id());
                self.child = Some((child, now));
            }
            Err(err) => {
                log::warn!(
                    "tailnet: starting {}: {err}; trying again in {:?}",
                    self.program.display(),
                    self.wait
                );
                self.wait_before_next(now);
            }
        }
    }

    /// The next start is not before the wait, and the one after that waits
    /// twice as long.
    fn wait_before_next(&mut self, now: Instant) {
        self.next_start = now + self.wait;
        self.wait = (self.wait * 2).min(RESTART_MAX);
    }
}

/// Starts the program, on its own: no terminal, and a process group of its
/// own, so that a signal meant for the server's group is not meant for it.
/// It ends with this process all the same, which it is told is its parent
/// (see thinkterm-tailnet-node).
#[cfg(unix)]
fn start(program: &Path) -> std::io::Result<Child> {
    use std::os::unix::process::CommandExt;
    Command::new(program)
        .arg("--parent")
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
}

/// Starts the program, on its own: no window, and a console of its own
/// rather than the server's, so that a Ctrl-C meant for that is not meant
/// for it, as the plugin host is started. It ends with this process all the
/// same, which it is told is its parent (see thinkterm-tailnet-node).
#[cfg(windows)]
fn start(program: &Path) -> std::io::Result<Child> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new(program)
        .arg("--parent")
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program in `dir` that runs `script` as sh would.
    #[cfg(unix)]
    fn program(dir: &Path, name: &str, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A batch file in `dir` that does what `script` -- `sleep <secs>` or
    /// `exit <code>`, as the tests here say it -- would.
    #[cfg(windows)]
    fn program(dir: &Path, name: &str, script: &str) -> PathBuf {
        let path = dir.join(format!("{name}.cmd"));
        let batch = match script.split_once(' ') {
            // A ping a second, the first at once.
            Some(("sleep", secs)) => {
                let pings = secs.parse::<u32>().unwrap() + 1;
                format!("@ping -n {pings} 127.0.0.1 >nul")
            }
            Some(("exit", code)) => format!("@exit /b {code}"),
            _ => panic!("no batch file for {script:?}"),
        };
        std::fs::write(&path, format!("{batch}\r\n")).unwrap();
        path
    }

    /// Steps `keeper` at `now` until the program it ran has ended.
    fn until_ended(keeper: &mut Keeper, enabled: bool, now: Instant) {
        for _ in 0..100 {
            keeper.step(enabled, now);
            if keeper.child.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the program did not end");
    }

    fn stop(keeper: &mut Keeper) {
        if let Some((child, _)) = keeper.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    #[test]
    fn the_program_runs_while_the_tailnet_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut keeper = Keeper::new(program(dir.path(), "stays", "sleep 5"));
        let now = Instant::now();
        keeper.step(false, now);
        assert!(keeper.child.is_none(), "started while the tailnet is off");
        keeper.step(true, now);
        let pid = keeper.child.as_ref().expect("not started").0.id();
        keeper.step(true, now);
        assert_eq!(keeper.child.as_ref().unwrap().0.id(), pid, "started twice");
        stop(&mut keeper);
    }

    #[test]
    fn a_program_that_keeps_ending_waits_longer_each_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut keeper = Keeper::new(program(dir.path(), "ends", "exit 3"));
        let mut now = Instant::now();
        let mut waits = vec![];
        for _ in 0..8 {
            keeper.step(true, now);
            assert!(keeper.child.is_some(), "not started once its wait was over");
            until_ended(&mut keeper, true, now);
            let wait = keeper.next_start - now;
            waits.push(wait.as_secs());
            // Not before the wait.
            keeper.step(true, now + wait - Duration::from_millis(1));
            assert!(keeper.child.is_none(), "started before its wait was over");
            now += wait;
        }
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60]);
    }

    #[test]
    fn a_program_that_ran_long_enough_starts_again_after_the_first_wait() {
        let dir = tempfile::tempdir().unwrap();
        let mut keeper = Keeper::new(program(dir.path(), "ends", "exit 3"));
        keeper.wait = RESTART_MAX;
        let started = Instant::now();
        keeper.step(true, started);
        until_ended(&mut keeper, true, started + RAN_LONG_ENOUGH);
        assert_eq!(
            keeper.next_start - (started + RAN_LONG_ENOUGH),
            RESTART_FIRST
        );
    }

    #[test]
    fn a_program_that_ended_as_the_tailnet_went_off_starts_at_once_when_it_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut keeper = Keeper::new(program(dir.path(), "ends", "exit 0"));
        keeper.wait = RESTART_MAX;
        let now = Instant::now();
        keeper.step(true, now);
        until_ended(&mut keeper, false, now);
        keeper.step(false, now);
        assert!(keeper.child.is_none(), "started while the tailnet is off");
        keeper.step(true, now);
        assert!(keeper.child.is_some(), "not started at once");
        until_ended(&mut keeper, false, now);
    }

    #[test]
    fn a_missing_program_is_tried_again_later() {
        let dir = tempfile::tempdir().unwrap();
        let mut keeper = Keeper::new(dir.path().join("missing"));
        let now = Instant::now();
        keeper.step(true, now);
        assert!(keeper.child.is_none());
        assert_eq!(keeper.next_start - now, RESTART_FIRST);
        keeper.step(true, now + RESTART_FIRST);
        assert_eq!(keeper.next_start - (now + RESTART_FIRST), RESTART_FIRST * 2);
    }

    #[test]
    fn a_connection_during_a_takeover_waits_for_the_predecessors_node_to_go() {
        let taking_over = AtomicBool::new(true);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(150));
                taking_over.store(false, Ordering::SeqCst);
            });
            let started = Instant::now();
            assert!(wait_while(&taking_over, Duration::from_secs(2), &|| false));
            assert!(started.elapsed() >= Duration::from_millis(150));
        });
        // Not for good, nor once the session is closed.
        let taking_over = AtomicBool::new(true);
        assert!(!wait_while(
            &taking_over,
            Duration::from_millis(50),
            &|| false
        ));
        assert!(!wait_while(&taking_over, Duration::from_secs(2), &|| true));
        assert!(wait_while(&AtomicBool::new(false), Duration::ZERO, &|| {
            false
        }));
    }
}
