//! Python for the plugins that are a Python script (`[run] python`): the
//! Python the user installed, found on this machine. ThinkTerm ships none.
//!
//! It is looked for where the user's own terminal would find it -- the
//! PATH of their login shell, which a ThinkTerm started from the Dock or a
//! desktop menu does not have -- then on the host's PATH, then where the
//! usual installers put it. Each candidate is asked its version, and the
//! first that is [`OLDEST`] or later is run as the program it says it is
//! (`sys.executable`), so a launcher or a shim found first is not between
//! the host and the plugin.
//!
//! Some candidates must not be run to ask: on a Mac without Apple's
//! command line tools, `/usr/bin/python3` puts up a dialog offering to
//! install them; and the Python install manager on Windows installs a
//! Python when it has none, unless told not to. On Windows, the Store's
//! stand-in `python.exe` answers a version check with an error rather than
//! opening the Store, so it is asked like any other.
//!
//! Looking runs a login shell and the candidates, which takes a while: it
//! is done on a thread of its own, never with the host locked, and the host
//! is told when it is done ([`when_looked`]). Meanwhile a plugin waits.
//! What was found is kept while its file stays as it was; not finding one
//! is kept for [`MISSING_FOR`], or until a plugin is reloaded ([`forget`]),
//! then looked into again, so a Python installed since is found.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::stamp::{stamp, Stamp};

/// The oldest Python a script plugin runs on.
pub const OLDEST: (u32, u32) = (3, 8);

/// What a script plugin's Python is given besides what every plugin is: its
/// standard streams unbuffered and in UTF-8 whatever the system's code page,
/// and no compiled files written beside the plugin's own.
pub const ENV: [(&str, &str); 4] = [
    ("PYTHONUNBUFFERED", "1"),
    ("PYTHONUTF8", "1"),
    ("PYTHONIOENCODING", "utf-8"),
    ("PYTHONDONTWRITEBYTECODE", "1"),
];
/// What a script plugin's Python is not given of the host's environment:
/// what would make it another Python than the one asked, which was asked
/// without them (`-I`).
pub const UNSET: [&str; 2] = ["PYTHONHOME", "PYTHONPATH"];

/// How long one candidate has to say its version.
const ASK_WITHIN: Duration = Duration::from_secs(5);
/// How long the login shell has to say its PATH. A profile can wait on
/// something interactive forever; the other places are looked in then.
const SHELL_WITHIN: Duration = Duration::from_secs(5);
/// How long a Python not found stays not found, unless a plugin is
/// reloaded: a plugin waiting on show does not have the candidates asked
/// again and again.
const MISSING_FOR: Duration = Duration::from_secs(30);
/// How long the rest of what a program printed is waited for once it has
/// exited: something it started may hold its output open for good.
const LAST_OUTPUT_WITHIN: Duration = Duration::from_millis(300);
/// What a candidate is asked: its version, then the program it is -- in
/// UTF-8 whatever the system's code page, which `-I` would otherwise have
/// it print in.
const QUESTION: &str = "import sys; sys.stdout.buffer.write(('%d %d\\n%s\\n' % \
     (sys.version_info[0], sys.version_info[1], sys.executable)).encode('utf-8', 'surrogateescape'))";

/// A Python found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The program it is, as it says: what runs the script.
    pub program: PathBuf,
    pub version: (u32, u32),
}

/// What looking came to, when, and the stamp of what was found.
struct Looked {
    result: Result<Found, String>,
    at: Instant,
    stamp: Option<Stamp>,
}

/// What is known of the Python, and whether it is being looked for now.
struct Known {
    looked: Option<Looked>,
    looking: bool,
}

static KNOWN: Mutex<Known> = Mutex::new(Known {
    looked: None,
    looking: false,
});
/// Who is told once Python has been looked for.
static WHEN_LOOKED: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// What finding Python came to, as far as it is known now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Found(Found),
    /// There is none, and why: a reason a person reads, saying how to
    /// install one.
    Missing(String),
    /// It is being looked for.
    Looking,
}

/// Has `told` called each time Python has been looked for: a plugin waiting
/// for it can go on.
pub fn when_looked(told: impl Fn() + Send + Sync + 'static) {
    let _ = WHEN_LOOKED.set(Box::new(told));
}

/// The Python script plugins run on, as last found, without waiting: when it
/// is not known -- not looked for yet, or not lately enough -- it is looked
/// for on a thread of its own, and this is [`Lookup::Looking`] until then.
pub fn lookup() -> Lookup {
    let mut known = KNOWN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(looked) = known.looked.as_ref() {
        match &looked.result {
            Ok(found) if stamp(&found.program) == looked.stamp => {
                return Lookup::Found(found.clone())
            }
            Err(why) if looked.at.elapsed() < MISSING_FOR => return Lookup::Missing(why.clone()),
            _ => {}
        }
    }
    if !known.looking {
        let looking = std::thread::Builder::new()
            .name("python-lookup".into())
            .stack_size(crate::process::STACK)
            .spawn(look_and_tell);
        if let Err(err) = looking {
            return Lookup::Missing(format!("cannot look for Python: {err}"));
        }
        known.looking = true;
    }
    Lookup::Looking
}

/// Forgets what was found: the next plugin to need Python has it looked for
/// again, at once. A plugin is reloaded, say, after Python was installed.
pub fn forget() {
    let mut known = KNOWN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    known.looked = None;
}

fn look_and_tell() {
    let result = look();
    match &result {
        Ok(found) => log::info!(
            "script plugins run on Python {}.{}: {}",
            found.version.0,
            found.version.1,
            found.program.display()
        ),
        Err(why) => log::warn!("script plugins cannot run: {why}"),
    }
    {
        let mut known = KNOWN
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        known.looked = Some(Looked {
            stamp: result.as_ref().ok().and_then(|found| stamp(&found.program)),
            result,
            at: Instant::now(),
        });
        known.looking = false;
    }
    // Told with nothing of this module locked: the host locks itself.
    if let Some(told) = WHEN_LOOKED.get() {
        told();
    }
}

fn look() -> Result<Found, String> {
    let mut asked: Vec<PathBuf> = Vec::new();
    let mut passed_over: Vec<String> = Vec::new();
    for dir in places() {
        for (name, launcher) in CANDIDATES {
            let path = dir.join(name);
            if !is_program(&path) {
                continue;
            }
            let real = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            // A shim is asked once whichever directory it is found in;
            // links to one Python under several names are one Python.
            if asked.contains(&real) || asked.contains(&path) {
                continue;
            }
            asked.push(real.clone());
            asked.push(path.clone());
            if let Some(why) = not_to_ask(&path, &real) {
                passed_over.push(why);
                continue;
            }
            match ask(&path, launcher) {
                Ok(found) if found.version >= OLDEST => return Ok(found),
                Ok(found) => passed_over.push(format!(
                    "{} is Python {}.{}",
                    path.display(),
                    found.version.0,
                    found.version.1
                )),
                // Most often a stand-in for a Python not installed: not
                // worth a word unless nothing else is found either.
                Err(why) => log::info!("passing over {}: {why}", path.display()),
            }
        }
    }
    Err(missing(&passed_over))
}

/// Why there is no Python to run a script with, as the user is to read it.
fn missing(passed_over: &[String]) -> String {
    let mut why = format!(
        "it needs Python {}.{} or later, and none was found",
        OLDEST.0, OLDEST.1
    );
    if !passed_over.is_empty() {
        why.push_str(&format!(" ({})", passed_over.join("; ")));
    }
    format!("{why}: {}, then reload the plugin", how_to_install())
}

fn how_to_install() -> &'static str {
    if cfg!(target_os = "macos") {
        "install one from python.org or with Homebrew (brew install python), \
         or Apple's command line tools (xcode-select --install)"
    } else if cfg!(windows) {
        "install one from python.org, or with winget install Python.Python.3.12"
    } else {
        "install python3 with the system's package manager"
    }
}

/// The names a Python goes by, each with what goes before the question:
/// the Windows launcher is asked for a Python 3.
#[cfg(not(windows))]
const CANDIDATES: [(&str, &[&str]); 2] = [("python3", &[]), ("python", &[])];
#[cfg(windows)]
const CANDIDATES: [(&str, &[&str]); 3] = [
    ("py.exe", &["-3"]),
    ("python.exe", &[]),
    ("python3.exe", &[]),
];

/// Why the candidate at `path` -- really `real`, links followed -- is not
/// even asked, if it is not.
fn not_to_ask(path: &Path, real: &Path) -> Option<String> {
    let apples =
        |at: &Path| at == Path::new("/usr/bin/python3") || at == Path::new("/usr/bin/python");
    if cfg!(target_os = "macos") && (apples(path) || apples(real)) && !apple_tools_installed() {
        return Some(format!(
            "{} needs Apple's command line tools",
            path.display()
        ));
    }
    None
}

/// Whether the developer directory Apple's `/usr/bin/python3` hands over to
/// has a Python: without one, running it offers to install the tools.
fn apple_tools_installed() -> bool {
    let Ok(out) = Command::new("/usr/bin/xcode-select")
        .arg("-p")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    let developer = String::from_utf8_lossy(&out.stdout);
    out.status.success()
        && Path::new(developer.trim())
            .join("usr")
            .join("bin")
            .join("python3")
            .is_file()
}

/// Asks the Python at `program` -- with `launcher`'s arguments first -- its
/// version and the program it is.
fn ask(program: &Path, launcher: &[&str]) -> Result<Found, String> {
    let mut command = Command::new(program);
    command
        .args(launcher)
        .args(["-I", "-c", QUESTION])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // The Python install manager installs a Python when it has none,
        // which is not for a question to do.
        .env("PYTHON_MANAGER_AUTOMATIC_INSTALL", "false");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let said = run_within(&mut command, ASK_WITHIN)?;
    let said = String::from_utf8_lossy(&said);
    let found = read_answer(&said).ok_or_else(|| format!("it answered {said:?}"))?;
    // The program it says it is, when that is there to run; else the one
    // asked, unless that was a launcher, which needs its arguments.
    let program = match found.program {
        program if program.is_file() => program,
        _ if launcher.is_empty() => program.to_path_buf(),
        _ => return Err("it did not say which program it is".into()),
    };
    Ok(Found {
        program,
        version: found.version,
    })
}

/// What a Python said to [`QUESTION`].
fn read_answer(said: &str) -> Option<Found> {
    let mut lines = said.lines();
    let mut version = lines.next()?.split_whitespace();
    let major = version.next()?.parse().ok()?;
    let minor = version.next()?.parse().ok()?;
    let program = PathBuf::from(lines.next().unwrap_or_default().trim());
    Some(Found {
        program,
        version: (major, minor),
    })
}

/// Runs `command` for what it prints, killing it after `within`. What it
/// prints is read on a thread of its own, and taken as it is a moment after
/// it exits: something it started may hold its output open for good, and
/// what it printed itself is in by then.
fn run_within(command: &mut Command, within: Duration) -> Result<Vec<u8>, String> {
    let mut child = command.spawn().map_err(|err| format!("{err}"))?;
    let stdout = child.stdout.take();
    let said: Arc<Mutex<Vec<u8>>> = Arc::default();
    let ended = Arc::new(AtomicBool::new(false));
    let reader = std::thread::Builder::new()
        .name("python-ask".into())
        .stack_size(crate::process::STACK)
        .spawn({
            let said = Arc::clone(&said);
            let ended = Arc::clone(&ended);
            move || {
                if let Some(mut stdout) = stdout {
                    let mut chunk = [0u8; 4096];
                    loop {
                        match stdout.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(read) => {
                                let mut said = said.lock().unwrap_or_else(|e| e.into_inner());
                                if said.len() < 64 * 1024 {
                                    said.extend_from_slice(&chunk[..read]);
                                }
                            }
                        }
                    }
                }
                ended.store(true, Ordering::Release);
            }
        });
    if reader.is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return Err("cannot read what it says".into());
    }
    let deadline = Instant::now() + within;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let last = Instant::now() + LAST_OUTPUT_WITHIN;
                while !ended.load(Ordering::Acquire) && Instant::now() < last {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let said = std::mem::take(&mut *said.lock().unwrap_or_else(|e| e.into_inner()));
                return if said.is_empty() {
                    Err("it said nothing".to_string())
                } else {
                    Ok(said)
                };
            }
            Ok(Some(status)) => return Err(format!("it ended: {status}")),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("it did not answer within {within:?}"));
            }
            Err(err) => return Err(format!("waiting for it: {err}")),
        }
    }
}

fn is_program(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        // A Store app's alias is a link that cannot be followed, and runs.
        return cfg!(windows) && std::fs::symlink_metadata(path).is_ok();
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    meta.is_file()
}

/// Where Python is looked for, in order, each once: the login shell's
/// PATH, the host's own PATH, and where the usual installers put it.
fn places() -> Vec<PathBuf> {
    let mut places: Vec<PathBuf> = Vec::new();
    let mut add = |dir: PathBuf| {
        if !dir.as_os_str().is_empty() && !places.contains(&dir) {
            places.push(dir);
        }
    };
    for dir in shell_path() {
        add(dir);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            add(dir);
        }
    }
    for dir in installed_places() {
        add(dir);
    }
    places
}

/// Where the usual installers put a Python, newest first where there are
/// several.
#[cfg(not(windows))]
fn installed_places() -> Vec<PathBuf> {
    let mut places = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    places.push(PathBuf::from("/opt/homebrew/bin"));
    places.push(PathBuf::from("/usr/local/bin"));
    if let Some(home) = &home {
        places.push(home.join(".pyenv").join("shims"));
        places.push(home.join(".local").join("bin"));
    }
    if cfg!(target_os = "macos") {
        // python.org's installer.
        places.push(PathBuf::from(
            "/Library/Frameworks/Python.framework/Versions/Current/bin",
        ));
    }
    places.push(PathBuf::from("/usr/bin"));
    places.push(PathBuf::from("/bin"));
    places
}

#[cfg(windows)]
fn installed_places() -> Vec<PathBuf> {
    let mut places = Vec::new();
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    if let Some(local) = &local {
        // The Python install manager's.
        places.push(local.join("Python").join("bin"));
        places.push(local.join("Programs").join("Python").join("Launcher"));
    }
    if let Some(windows) = std::env::var_os("SystemRoot") {
        // The launcher python.org's installer puts for every user.
        places.push(PathBuf::from(windows));
    }
    let versions = |root: PathBuf| -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&root)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .to_ascii_lowercase()
                            .starts_with("python3")
                    })
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default();
        // Python313 before Python39: by the number, not the name.
        found.sort_by_key(|dir| {
            let name = dir
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let digits: String = name.chars().filter(char::is_ascii_digit).collect();
            std::cmp::Reverse(digits.get(1..).and_then(|minor| minor.parse::<u32>().ok()))
        });
        found
    };
    if let Some(local) = &local {
        places.extend(versions(local.join("Programs").join("Python")));
    }
    if let Some(programs) = std::env::var_os("ProgramFiles") {
        places.extend(versions(PathBuf::from(programs)));
    }
    if let Some(local) = &local {
        // The Store's, and its stand-ins.
        places.push(local.join("Microsoft").join("WindowsApps"));
    }
    places
}

/// The login shell's PATH, looked for once, on a thread of its own: the
/// first script plugin waits for it, for as long as [`SHELL_WITHIN`].
struct ShellPath {
    started: bool,
    dirs: Option<Vec<PathBuf>>,
}

static SHELL_PATH: Mutex<ShellPath> = Mutex::new(ShellPath {
    started: false,
    dirs: None,
});
static SHELL_SAID: Condvar = Condvar::new();

fn shell_path() -> Vec<PathBuf> {
    let mut shell = SHELL_PATH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !shell.started {
        shell.started = true;
        let asking = std::thread::Builder::new()
            .name("login-shell-path".into())
            .stack_size(crate::process::STACK)
            .spawn(|| {
                let dirs: Vec<PathBuf> = login_shell_path()
                    .map(|path| std::env::split_paths(&path).collect())
                    .unwrap_or_default();
                let mut shell = SHELL_PATH
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                shell.dirs = Some(dirs);
                SHELL_SAID.notify_all();
            });
        if asking.is_err() {
            shell.dirs = Some(Vec::new());
        }
    }
    let deadline = Instant::now() + SHELL_WITHIN + Duration::from_millis(500);
    while shell.dirs.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        shell = match SHELL_SAID.wait_timeout(shell, left) {
            Ok((shell, _)) => shell,
            Err(poisoned) => poisoned.into_inner().0,
        };
    }
    shell.dirs.clone().unwrap_or_default()
}

/// What `PATH` the user's login shell sets, as their terminal has it.
#[cfg(unix)]
fn login_shell_path() -> Option<String> {
    let shell = user_shell()?;
    let mut command = Command::new(&shell);
    command
        .args(["-l", "-c", "printf %s \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    match run_within(&mut command, SHELL_WITHIN) {
        Ok(path) if !String::from_utf8_lossy(&path).trim().is_empty() => {
            Some(String::from_utf8_lossy(&path).trim().to_string())
        }
        Ok(_) => None,
        Err(why) => {
            log::warn!("the login shell {shell} did not say its PATH: {why}");
            None
        }
    }
}

#[cfg(not(unix))]
fn login_shell_path() -> Option<String> {
    None
}

/// The user's shell: `$SHELL` when it names one that runs, else the one
/// the password database has for them.
#[cfg(unix)]
fn user_shell() -> Option<String> {
    if let Some(shell) = std::env::var_os("SHELL") {
        if is_program(Path::new(&shell)) {
            return Some(shell.to_string_lossy().into_owned());
        }
    }
    // SAFETY: getpwuid's entry is read before anything else can call it on
    // this thread, and copied out.
    let shell = unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() || (*entry).pw_shell.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr((*entry).pw_shell)
            .to_string_lossy()
            .into_owned()
    };
    is_program(Path::new(&shell)).then_some(shell)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pythons_answer_says_its_version_and_program() {
        assert_eq!(
            read_answer("3 12\n/usr/bin/python3.12\n"),
            Some(Found {
                program: "/usr/bin/python3.12".into(),
                version: (3, 12)
            })
        );
        assert_eq!(
            read_answer("3 8\r\nC:\\Python38\\python.exe\r\n").map(|found| found.version),
            Some((3, 8))
        );
        assert_eq!(read_answer("Python was not found"), None);
        assert_eq!(read_answer(""), None);
        assert!((3, 12) >= OLDEST && (3, 8) >= OLDEST && (3, 7) < OLDEST && (2, 7) < OLDEST);
    }

    #[test]
    fn not_finding_python_says_how_to_install_it() {
        let why = missing(&["/usr/bin/python3 is Python 3.6".into()]);
        assert!(
            why.starts_with("it needs Python 3.8 or later, and none was found"),
            "{why}"
        );
        assert!(why.contains("(/usr/bin/python3 is Python 3.6)"), "{why}");
        assert!(why.contains("install"), "{why}");
        assert!(why.ends_with("then reload the plugin"), "{why}");
        // Nothing passed over: nothing said of it.
        assert!(
            missing(&[]).starts_with("it needs Python 3.8 or later, and none was found: "),
            "{}",
            missing(&[])
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_candidate_says_its_version_or_is_passed_over() {
        let dir = tempfile::tempdir().unwrap();
        let fake = |name: &str, body: &str| {
            use std::os::unix::fs::PermissionsExt;
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let old = fake("old", "echo 3 6; echo");
        let found = ask(&old, &[]).unwrap();
        assert_eq!(found.version, (3, 6));
        assert_eq!(found.program, old, "the one asked, when it names none");
        let stand_in = fake("stand-in", "echo 'Python was not found' >&2; exit 9009");
        assert!(ask(&stand_in, &[]).unwrap_err().contains("ended"));
        let mute = fake("mute", "echo nonsense");
        assert!(ask(&mute, &[]).unwrap_err().contains("answered"));
        // What it printed is taken though something it started keeps its
        // output open.
        let holds = fake("holds", "echo 3 9; echo; sleep 3 &");
        let started = Instant::now();
        assert_eq!(ask(&holds, &[]).unwrap().version, (3, 9));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
