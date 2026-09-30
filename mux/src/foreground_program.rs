//! Which program leads a pane's terminal, observed where the pane lives.
//!
//! Like [`crate::agent_status`], this runs in the process that owns the
//! pane: the GUI's own mux for its local panes, a mux server for everything
//! it hosts. Clients receive the result as `ForegroundProgramChanged` and
//! answer [`Pane::foreground_program`] from what was pushed, so a remote
//! pane reports exactly what a local one would.
//!
//! Only facts are decided here: the executable leading the terminal's
//! foreground process group and, for an interpreter, shell or launcher,
//! what it runs. What a program looks like is the frontend's business.
//!
//! Nothing is polled. A pane is looked at when it prints or retitles, and
//! once more after that burst settles; a pane doing nothing costs nothing.

use crate::pane::{CachePolicy, Pane, PaneId};
use crate::{Mux, MuxNotification};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use thinkterm_proto::ForegroundProgram;

/// Batching window after the first mark: every pane marked within it is
/// looked at in one pass.
const DRAIN_DELAY: Duration = Duration::from_millis(250);
/// A pane is looked at no more often than this however much it prints: the
/// leader rarely changes mid-stream, and a build scrolling past would
/// otherwise pay a process probe per burst. Windows reads the whole process
/// table for every look, on this thread, so it looks less often.
#[cfg(not(windows))]
const PROBE_FLOOR: Duration = Duration::from_millis(750);
#[cfg(windows)]
const PROBE_FLOOR: Duration = Duration::from_secs(2);
/// The same for panes no window is showing: their tabs are out of sight,
/// and being late there is free.
const BACKGROUND_PROBE_FLOOR: Duration = Duration::from_secs(5);
/// How long a new observation must hold before it is published, so a
/// command that is over in a moment (`ls`, `git status`) never flashes.
const CONFIRM: Duration = Duration::from_millis(500);
/// How many forced process probes one pass may make. Each blocks this
/// thread for most of a millisecond; a server taking over a hundred panes
/// spreads its first looks over a few passes instead of stalling once.
const FRESH_LOOKS_PER_DRAIN: usize = 8;
/// How deep launchers may nest (`sudo env FOO=1 nice vim`).
const MAX_UNWRAP_DEPTH: usize = 4;

#[derive(Default)]
struct Record {
    /// What consumers currently see.
    published: Option<ForegroundProgram>,
    /// Whether anything has gone out yet. A pane's first observation is
    /// published at once, so a new tab does not wait out the confirmation.
    announced: bool,
    /// An observation that differs from `published`, and when it was made.
    candidate: Option<(Option<ForegroundProgram>, Instant)>,
    /// When the pane was last looked at, for the floor.
    probed_at: Option<Instant>,
}

lazy_static::lazy_static! {
    static ref REGISTRY: RwLock<HashMap<PaneId, Record>> = RwLock::new(HashMap::new());
    /// Panes waiting to be looked at, each with whether its look was
    /// prompted by activity (`true`) or is the one owed after that
    /// activity settled (`false`), which sees what the burst left behind:
    /// a program that drew its screen and went quiet.
    static ref PENDING: Mutex<HashMap<PaneId, bool>> = Mutex::new(HashMap::new());
    static ref PROCESS_PREFERENCE: RwLock<Option<Box<dyn Fn() -> bool + Send + Sync>>> =
        RwLock::new(None);
}
static DRAIN_SCHEDULED: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// Cached gate consulted on every notification; see `refresh_enabled`.
static ENABLED: AtomicBool = AtomicBool::new(true);

/// The program this process last published for the pane, if any. This is
/// what `Pane::foreground_program`'s default body answers with.
pub fn program_for_pane(pane_id: PaneId) -> Option<ForegroundProgram> {
    REGISTRY
        .read()
        .get(&pane_id)
        .and_then(|record| record.published.clone())
}

/// Install a process-local switch. The GUI wires its tab-icon setting
/// through this so a user who turned the icons off pays nothing for them;
/// a mux server installs nothing and always observes, because its clients
/// decide for themselves what to show.
pub fn set_process_preference(preference: impl Fn() -> bool + Send + Sync + 'static) {
    *PROCESS_PREFERENCE.write() = Some(Box::new(preference));
    refresh_enabled();
}

/// Re-read the switch. The GUI calls this when its setting changes; the
/// flip itself is handled by the next drain.
pub fn refresh_enabled() {
    let enabled = PROCESS_PREFERENCE.read().as_ref().map_or(true, |f| f());
    let was = ENABLED.swap(enabled, Ordering::AcqRel);
    // Before `initialize_mux` there is nothing to withdraw and no
    // scheduler to withdraw it with; installation looks at every pane.
    if was == enabled || !INSTALLED.load(Ordering::Acquire) {
        return;
    }
    if enabled {
        mark_all_panes();
    } else {
        schedule_drain();
    }
}

/// Whether this process observes: the GUI's tab icon switch, or always
/// on a mux server.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Start observing. Idempotent; called once the main-thread scheduler
/// exists, by the GUI and by the mux server, next to the agent detector.
pub fn initialize_mux(mux: &Mux) {
    if !promise::spawn::is_scheduler_configured() {
        log::error!("foreground program observer deferred: no scheduler is configured");
        return;
    }
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    mux.subscribe(|notification| {
        if !enabled() {
            return true;
        }
        match notification {
            MuxNotification::PaneOutput(pane_id) | MuxNotification::PaneAdded(pane_id) => {
                mark_dirty(pane_id)
            }
            // A shell announcing the command it is about to run retitles
            // the terminal before the program has printed anything.
            MuxNotification::Alert {
                pane_id,
                alert:
                    wezterm_term::Alert::WindowTitleChanged(_)
                    | wezterm_term::Alert::TabTitleChanged(_)
                    | wezterm_term::Alert::IconTitleChanged(_),
            } => mark_dirty(pane_id),
            // Forget only, never publish: this runs inside Mux::notify, and
            // clients learn of the pane's end from PaneRemoved itself.
            MuxNotification::PaneRemoved(pane_id) => {
                REGISTRY.write().remove(&pane_id);
            }
            _ => {}
        }
        true
    });
    // Panes that exist already (a restored session, a server taking over)
    // announced themselves before anyone was listening.
    mark_all_panes();
}

fn mark_all_panes() {
    if let Some(mux) = Mux::try_get() {
        for pane in mux.iter_panes() {
            mark_dirty(pane.pane_id());
        }
    }
}

fn mark_dirty(pane_id: PaneId) {
    PENDING.lock().insert(pane_id, true);
    schedule_drain();
}

/// Queue a look again, keeping any activity that arrived meanwhile.
fn requeue(pane_id: PaneId, activity: bool) {
    *PENDING.lock().entry(pane_id).or_insert(activity) |= activity;
}

/// Whether a look must read the process table afresh rather than take the
/// cached leader, whose background refresh may not have run yet: a pane's
/// first look (nothing else warms the cache on a headless server), a
/// confirmation, and the look owed after activity must see the truth; a
/// look prompted by the activity itself makes do with the cache.
fn needs_fresh_look(known: bool, confirming: bool, activity: bool) -> bool {
    !known || confirming || !activity
}

fn schedule_drain() {
    if DRAIN_SCHEDULED.swap(true, Ordering::AcqRel) {
        return;
    }
    // spawn_into_main_thread, not spawn: PaneOutput arrives on the pty
    // parser threads, where no local scheduler exists.
    promise::spawn::spawn_into_main_thread(async {
        smol::Timer::after(DRAIN_DELAY).await;
        DRAIN_SCHEDULED.store(false, Ordering::Release);
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(drain)).is_err() {
            log::error!("foreground program observation panicked; observation continues");
        }
    })
    .detach();
}

/// Tell consumers the pane's program changed. Always from a fresh task:
/// publication is reachable from inside Mux subscriber callbacks, where a
/// synchronous notify would re-enter the subscriber lock.
fn publish_change(pane_id: PaneId) {
    promise::spawn::spawn_into_main_thread(async move {
        if let Some(mux) = Mux::try_get() {
            mux.notify(MuxNotification::ForegroundProgramChanged(pane_id));
        }
    })
    .detach();
}

fn forget(pane_id: PaneId) -> bool {
    REGISTRY
        .write()
        .remove(&pane_id)
        .and_then(|record| record.published)
        .is_some()
}

fn drain() {
    if !enabled() {
        // Switched off: withdraw everything, so no stale icon survives.
        let ids: Vec<PaneId> = REGISTRY.read().keys().copied().collect();
        for pane_id in ids {
            if forget(pane_id) {
                publish_change(pane_id);
            }
        }
        PENDING.lock().clear();
        return;
    }
    let pending: Vec<(PaneId, bool)> = PENDING.lock().drain().collect();
    let Some(mux) = Mux::try_get() else {
        return;
    };
    let now = Instant::now();
    let mut fresh_looks = 0;
    for (pane_id, activity) in pending {
        let Some(pane) = mux.get_pane(pane_id) else {
            if forget(pane_id) {
                publish_change(pane_id);
            }
            continue;
        };
        // A mirror's program is observed by its server and pushed to us.
        if pane.is_remote_mirror() {
            continue;
        }
        if pane.is_dead() {
            if forget(pane_id) {
                publish_change(pane_id);
            }
            continue;
        }
        let (known, confirming, since_probe) = match REGISTRY.read().get(&pane_id) {
            Some(record) => (
                true,
                record.candidate.is_some(),
                record.probed_at.map(|at| now.saturating_duration_since(at)),
            ),
            None => (false, false, None),
        };
        let wait = if confirming {
            CONFIRM
        } else if crate::agent_status::pane_is_foreground(&mux, pane_id) {
            PROBE_FLOOR
        } else {
            BACKGROUND_PROBE_FLOOR
        };
        if since_probe.is_some_and(|since| since < wait) {
            // Not dropped: the mark that brought it here may be its last.
            requeue(pane_id, activity);
            continue;
        }
        let policy = if needs_fresh_look(known, confirming, activity) {
            if fresh_looks == FRESH_LOOKS_PER_DRAIN {
                requeue(pane_id, activity);
                continue;
            }
            fresh_looks += 1;
            CachePolicy::FetchImmediate
        } else {
            CachePolicy::AllowStale
        };
        let observed = probe(pane.as_ref(), policy);
        let (changed, confirming) = {
            let mut registry = REGISTRY.write();
            let record = registry.entry(pane_id).or_default();
            let changed = observe(record, observed, now);
            (changed, record.candidate.is_some())
        };
        if changed {
            publish_change(pane_id);
        }
        // Activity owes a look after it; a candidate waits for its
        // confirmation. Neither is new activity.
        if activity || confirming {
            requeue(pane_id, false);
        }
    }
    if !PENDING.lock().is_empty() {
        schedule_drain();
    }
}

/// Fold one look into the pane's record. Returns whether the published
/// program changed.
fn observe(record: &mut Record, observed: Option<ForegroundProgram>, now: Instant) -> bool {
    record.probed_at = Some(now);
    if !record.announced {
        record.announced = true;
        record.candidate = None;
        let changed = record.published != observed;
        record.published = observed;
        return changed;
    }
    if observed == record.published {
        record.candidate = None;
        return false;
    }
    match &record.candidate {
        Some((candidate, since))
            if *candidate == observed && now.saturating_duration_since(*since) >= CONFIRM =>
        {
            record.published = observed;
            record.candidate = None;
            true
        }
        Some((candidate, _)) if *candidate == observed => false,
        _ => {
            record.candidate = Some((observed, now));
            false
        }
    }
}

fn probe(pane: &dyn Pane, policy: CachePolicy) -> Option<ForegroundProgram> {
    let path = pane
        .get_foreground_process_name(policy)
        .map(crate::agent_status::identify::normalize_executable_path)?;
    let executable = file_name(&path)?.to_string();
    let runs = if unwraps(&executable) {
        // AllowStale: the name probe above just refreshed the leader, so
        // this reads the same pid rather than racing a newer one.
        pane.get_foreground_process_argv(CachePolicy::AllowStale)
            .and_then(|argv| run_target(&executable, &argv, 0))
    } else {
        None
    };
    let program = ForegroundProgram { executable, runs };
    program.within_budget().then_some(program)
}

// ---------------------------------------------------------------------------
// What an interpreter, shell or launcher runs. No IO below this line: the
// executable and its argv come in, a file name comes out. The usual edits --
// a new launcher, a new interpreter, an option that takes a value -- are
// table changes here.

/// Launchers run the command named after their own options. Each entry
/// lists the options that consume the next argument, and how many plain
/// arguments come before the command (`timeout 5 vim`).
const LAUNCHERS: &[(&str, &[&str], usize)] = &[
    (
        "sudo",
        &[
            "-u",
            "-g",
            "-C",
            "-D",
            "-h",
            "-p",
            "-r",
            "-t",
            "-U",
            "-T",
            "--user",
            "--group",
            "--chdir",
            "--host",
            "--prompt",
            "--role",
            "--type",
            "--other-user",
            "--command-timeout",
            "--close-from",
        ],
        0,
    ),
    ("doas", &["-u", "-C"], 0),
    (
        "env",
        &["-u", "-C", "-S", "--unset", "--chdir", "--split-string"],
        0,
    ),
    ("nice", &["-n", "--adjustment"], 0),
    ("nohup", &[], 0),
    ("time", &["-f", "-o", "--format", "--output"], 0),
    ("caffeinate", &["-t", "-w"], 0),
    ("timeout", &["-s", "-k", "--signal", "--kill-after"], 1),
    ("stdbuf", &["-i", "-o", "-e"], 0),
    (
        "ionice",
        &["-c", "-n", "-p", "--class", "--classdata", "--pid"],
        0,
    ),
    ("xcrun", &["--sdk", "--toolchain"], 0),
    ("arch", &[], 0),
];

/// Interpreters and shells run the script named by their first plain
/// argument. `eval` flags mean the code came inline and there is no script
/// to name; a `module` flag names what runs instead (`python -m pytest`);
/// `subcommands` are stepped over (`deno run main.ts`).
struct Interpreter {
    names: &'static [&'static str],
    eval: &'static [&'static str],
    module: &'static [&'static str],
    takes_value: &'static [&'static str],
    subcommands: &'static [&'static str],
}

const INTERPRETERS: &[Interpreter] = &[
    Interpreter {
        names: &["node", "nodejs"],
        eval: &["-e", "--eval", "-p", "--print"],
        module: &[],
        takes_value: &[
            "-r",
            "--require",
            "--loader",
            "--import",
            "--experimental-loader",
            "--inspect-port",
            "--title",
            "--env-file",
        ],
        subcommands: &[],
    },
    Interpreter {
        names: &["bun"],
        eval: &["-e", "--eval", "-p", "--print"],
        module: &[],
        takes_value: &["-r", "--preload", "--cwd", "--env-file"],
        subcommands: &["run", "x", "exec", "test"],
    },
    Interpreter {
        names: &["deno"],
        eval: &["eval"],
        module: &[],
        takes_value: &["--config", "-c", "--import-map", "--env-file", "--location"],
        subcommands: &["run", "task", "test", "serve", "x"],
    },
    Interpreter {
        names: &["python", "pypy"],
        eval: &["-c"],
        module: &["-m"],
        takes_value: &["-W", "-X", "-Q"],
        subcommands: &[],
    },
    Interpreter {
        names: &["ruby"],
        eval: &["-e"],
        module: &[],
        takes_value: &["-r", "-I", "-C", "-E", "-F", "--encoding"],
        subcommands: &[],
    },
    Interpreter {
        names: &["perl"],
        eval: &["-e", "-E"],
        module: &[],
        takes_value: &["-I"],
        subcommands: &[],
    },
    Interpreter {
        names: &["php"],
        eval: &["-r"],
        module: &[],
        takes_value: &["-c", "-d", "-z"],
        subcommands: &[],
    },
    Interpreter {
        names: &["lua", "luajit"],
        eval: &["-e"],
        module: &[],
        takes_value: &["-l"],
        subcommands: &[],
    },
    Interpreter {
        names: &[
            "sh", "bash", "zsh", "fish", "dash", "ksh", "mksh", "ash", "yash", "tcsh", "csh", "nu",
        ],
        eval: &["-c", "--command"],
        module: &[],
        takes_value: &[
            "-o",
            "-O",
            "--rcfile",
            "--init-file",
            "--init-command",
            "-C",
        ],
        subcommands: &[],
    },
];

/// Whether `executable` is worth fetching argv for.
fn unwraps(executable: &str) -> bool {
    let name = interpreter_name(executable);
    launcher(&name).is_some()
        || interpreter(&name).is_some()
        || matches!(name.as_str(), "cmd" | "powershell" | "pwsh")
}

/// The name an executable is looked up by: lower case, without a Windows
/// extension, with a Python version folded away (`python3.12` → `python`).
fn interpreter_name(executable: &str) -> String {
    let mut name = executable.trim().to_lowercase();
    for suffix in [".exe", ".cmd", ".bat"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name.truncate(stripped.len());
            break;
        }
    }
    for python in ["python", "pypy"] {
        if let Some(version) = name.strip_prefix(python) {
            if version
                .split('.')
                .all(|part| part.chars().all(|ch| ch.is_ascii_digit()))
            {
                return python.to_string();
            }
        }
    }
    name
}

fn launcher(name: &str) -> Option<(&'static [&'static str], usize)> {
    LAUNCHERS
        .iter()
        .find(|(launcher, _, _)| *launcher == name)
        .map(|(_, takes_value, positional)| (*takes_value, *positional))
}

fn interpreter(name: &str) -> Option<&'static Interpreter> {
    INTERPRETERS
        .iter()
        .find(|interpreter| interpreter.names.contains(&name))
}

/// What `executable`, started as `argv`, runs; `None` when it runs nothing
/// that has a name of its own.
fn run_target(executable: &str, argv: &[String], depth: usize) -> Option<String> {
    if depth > MAX_UNWRAP_DEPTH {
        return None;
    }
    let name = interpreter_name(executable);
    if let Some((takes_value, positional)) = launcher(&name) {
        let rest = launched_command(argv, takes_value, positional)?;
        let command = file_name(rest.first()?.trim_matches(['"', '\'']))?;
        // The command may be an interpreter itself (`sudo python app.py`).
        return run_target(command, rest, depth + 1).or_else(|| Some(command.to_string()));
    }
    if let Some(interpreter) = interpreter(&name) {
        // A retitled Node process (`process.title = "claude"`) names itself
        // in argv[0], often with its script path clobbered.
        if matches!(name.as_str(), "node" | "nodejs" | "bun") {
            let title = argv
                .first()
                .and_then(|first| first.split_whitespace().next());
            if let Some(title) = title.and_then(file_name) {
                if interpreter_name(title) != name && launcher(title).is_none() {
                    return Some(title.to_string());
                }
            }
        }
        return script_argument(argv, interpreter);
    }
    match name.as_str() {
        "cmd" => cmd_target(argv),
        "powershell" | "pwsh" => powershell_target(argv),
        _ => None,
    }
}

/// The launched command and its own arguments: whatever follows the
/// launcher's options, `NAME=value` assignments and leading plain
/// arguments.
fn launched_command<'a>(
    argv: &'a [String],
    takes_value: &[&str],
    mut positional: usize,
) -> Option<&'a [String]> {
    let mut index = 1;
    while index < argv.len() {
        let arg = argv[index].as_str();
        if arg == "--" {
            return argv.get(index + 1..).filter(|rest| !rest.is_empty());
        }
        if arg.starts_with('-') && arg.len() > 1 {
            index += if takes_value.contains(&arg) { 2 } else { 1 };
            continue;
        }
        if arg.contains('=') && !arg.starts_with('=') {
            index += 1;
            continue;
        }
        if positional > 0 {
            positional -= 1;
            index += 1;
            continue;
        }
        return Some(&argv[index..]);
    }
    None
}

fn script_argument(argv: &[String], interpreter: &Interpreter) -> Option<String> {
    let mut args = argv.iter().skip(1);
    let mut stepped_over_subcommand = false;
    while let Some(arg) = args.next() {
        if arg == "--" {
            return args
                .next()
                .and_then(|token| file_name(token).map(str::to_string));
        }
        if flag_matches(arg, interpreter.eval) {
            return None;
        }
        if flag_matches(arg, interpreter.module) {
            // `-m pytest` or `-mpytest`.
            let module = match arg.strip_prefix(interpreter.module[0]) {
                Some(attached) if !attached.is_empty() => Some(attached),
                _ => args.next().map(String::as_str),
            };
            return module
                .filter(|module| !module.is_empty() && !module.starts_with('-'))
                .map(str::to_string);
        }
        if arg.starts_with('-') {
            if interpreter.takes_value.contains(&arg.as_str()) {
                let _ = args.next();
            }
            continue;
        }
        if !stepped_over_subcommand && interpreter.subcommands.contains(&arg.as_str()) {
            stepped_over_subcommand = true;
            continue;
        }
        return file_name(arg.trim_matches(['"', '\''])).map(str::to_string);
    }
    None
}

/// `-e`, `-epayload`, `--eval` and `--eval=payload` all count, and so does
/// a single-letter flag bundled with others (`bash -lc`, `perl -lne`).
fn flag_matches(arg: &str, flags: &[&str]) -> bool {
    flags.iter().any(|flag| {
        arg == *flag
            || (!flag.starts_with("--")
                && flag.starts_with('-')
                && arg.len() > flag.len()
                && arg.starts_with(flag))
            || (flag.starts_with("--")
                && arg.starts_with(flag)
                && arg.as_bytes().get(flag.len()) == Some(&b'='))
            || bundles(arg, flag)
    })
}

/// Whether `arg` bundles single-letter options (`-lc`) and one of them is
/// the single-letter `flag`.
fn bundles(arg: &str, flag: &str) -> bool {
    let Some(letter) = flag
        .strip_prefix('-')
        .filter(|letter| letter.len() == 1 && *letter != "-")
    else {
        return false;
    };
    arg.strip_prefix('-').is_some_and(|cluster| {
        cluster.len() > 1
            && cluster.bytes().all(|byte| byte.is_ascii_alphabetic())
            && cluster.contains(letter)
    })
}

/// `cmd /c C:\…\codex.cmd --model x`: the program is the first token of the
/// command text after `/c`.
fn cmd_target(argv: &[String]) -> Option<String> {
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.trim_matches('"').to_lowercase().as_str() {
            "/c" | "/k" => return args.next().and_then(|command| command_text_target(command)),
            flag if flag.starts_with('/') => continue,
            _ => return None,
        }
    }
    None
}

fn powershell_target(argv: &[String]) -> Option<String> {
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.trim_matches('"').to_lowercase().as_str() {
            "-file" | "-f" | "/file" => {
                return args
                    .next()
                    .and_then(|path| file_name(path).map(str::to_string))
            }
            "-command" | "-c" | "/command" | "/c" => {
                return args.next().and_then(|command| command_text_target(command))
            }
            // An encoded command is opaque on purpose; never guess.
            "-encodedcommand" | "-enc" | "/encodedcommand" | "/enc" => return None,
            "-configurationname" | "-executionpolicy" | "-outputformat" | "-psconsolefile"
            | "-version" | "-windowstyle" | "-workingdirectory" => {
                let _ = args.next();
            }
            flag if flag.starts_with('-') || flag.starts_with('/') => {}
            _ => return file_name(arg).map(str::to_string),
        }
    }
    None
}

/// The first real token of a command string, past the call forms (`&`,
/// `.`, `call`) that prefix the program.
fn command_text_target(command: &str) -> Option<String> {
    let mut rest = command;
    while let Some((token, after)) = first_token(rest) {
        rest = after;
        if token.is_empty() || token == "&" || token == "." || token.eq_ignore_ascii_case("call") {
            continue;
        }
        return file_name(token).map(str::to_string);
    }
    None
}

/// Extensions that end a Windows program's path.
const PROGRAM_PATH_ENDS: &[&str] = &[".exe", ".cmd", ".bat", ".com", ".ps1"];

/// The first token of command text, and the text after it. A quoted token
/// runs to its closing quote. A path runs to the extension that ends its
/// program, spaces and all: `C:\Program Files\nodejs\npm.cmd` arrives
/// unquoted once the command line has been split into argv. Anything else
/// ends at whitespace.
fn first_token(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    let quote = text.chars().next()?;
    if quote == '"' || quote == '\'' {
        let body = &text[1..];
        return Some(match body.find(quote) {
            Some(end) => (&body[..end], &body[end + 1..]),
            None => (body, ""),
        });
    }
    let word_end = text.find(char::is_whitespace).unwrap_or(text.len());
    if text[..word_end].contains(['\\', '/']) {
        let lower = text.to_ascii_lowercase();
        let program_end = PROGRAM_PATH_ENDS
            .iter()
            .filter_map(|ext| {
                lower
                    .match_indices(ext)
                    .map(|(at, _)| at + ext.len())
                    .find(|end| {
                        lower[*end..]
                            .chars()
                            .next()
                            .map_or(true, char::is_whitespace)
                    })
            })
            .min();
        if let Some(end) = program_end.filter(|end| *end > word_end) {
            return Some((&text[..end], &text[end..]));
        }
    }
    Some((&text[..word_end], &text[word_end..]))
}

/// The last component of a path, `None` for an empty one or an option.
fn file_name(path: &str) -> Option<&str> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path).trim();
    (!name.is_empty() && !name.starts_with('-')).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(executable: &str, argv: &[&str]) -> Option<String> {
        let argv: Vec<String> = argv.iter().map(|arg| arg.to_string()).collect();
        run_target(executable, &argv, 0)
    }

    #[test]
    fn interpreters_name_their_script() {
        assert_eq!(
            runs("node", &["node", "/opt/homebrew/bin/npm", "run", "dev"]).as_deref(),
            Some("npm")
        );
        assert_eq!(
            runs("node", &["node", "-r", "./preload.js", "server.js"]).as_deref(),
            Some("server.js")
        );
        assert_eq!(
            runs("bash", &["/bin/bash", "./deploy.sh"]).as_deref(),
            Some("deploy.sh")
        );
        assert_eq!(
            runs("python3.12", &["python3", "manage.py", "runserver"]).as_deref(),
            Some("manage.py")
        );
        assert_eq!(
            runs("Python", &["python3", "-m", "pytest", "-x"]).as_deref(),
            Some("pytest")
        );
        assert_eq!(
            runs("python3", &["python3", "-mhttp.server"]).as_deref(),
            Some("http.server")
        );
        assert_eq!(
            runs("deno", &["deno", "run", "-A", "main.ts"]).as_deref(),
            Some("main.ts")
        );
        assert_eq!(runs("bun", &["bun", "run", "dev"]).as_deref(), Some("dev"));
    }

    #[test]
    fn inline_code_and_interactive_shells_run_nothing_named() {
        assert_eq!(runs("zsh", &["-zsh"]), None);
        assert_eq!(runs("bash", &["bash", "--login"]), None);
        assert_eq!(runs("bash", &["bash", "-c", "make test"]), None);
        assert_eq!(runs("node", &["node", "-e", "console.log(1)"]), None);
        assert_eq!(runs("python3", &["python3", "-c", "print(1)"]), None);
        assert_eq!(runs("python3", &["python3"]), None);
    }

    #[test]
    fn a_bundled_eval_flag_is_still_inline_code() {
        assert_eq!(
            runs("bash", &["bash", "-lc", "cd /srv && ./deploy.sh"]),
            None
        );
        assert_eq!(runs("zsh", &["zsh", "-ic", "python app.py"]), None);
        assert_eq!(runs("perl", &["perl", "-lne", "print", "log.txt"]), None);
        // A bundle without the flag is just options.
        assert_eq!(
            runs("bash", &["bash", "-ex", "./deploy.sh"]).as_deref(),
            Some("deploy.sh")
        );
        assert_eq!(
            runs("python3", &["python3", "-Bm", "pytest"]).as_deref(),
            Some("pytest")
        );
    }

    #[test]
    fn launchers_unwrap_to_the_command_they_run() {
        assert_eq!(
            runs("sudo", &["sudo", "vim", "/etc/hosts"]).as_deref(),
            Some("vim")
        );
        assert_eq!(
            runs("sudo", &["sudo", "-u", "postgres", "psql"]).as_deref(),
            Some("psql")
        );
        assert_eq!(
            runs("env", &["env", "FOO=1", "python3", "-m", "pytest"]).as_deref(),
            Some("pytest")
        );
        assert_eq!(
            runs("timeout", &["timeout", "5", "htop"]).as_deref(),
            Some("htop")
        );
        assert_eq!(
            runs(
                "sudo",
                &["sudo", "env", "HOME=/root", "nice", "-n", "5", "vim"]
            )
            .as_deref(),
            Some("vim")
        );
        assert_eq!(runs("sudo", &["sudo", "-i"]), None);
    }

    #[test]
    fn retitled_node_processes_name_themselves() {
        assert_eq!(runs("node", &["claude"]).as_deref(), Some("claude"));
        assert_eq!(runs("node", &["node", "cli.js"]).as_deref(), Some("cli.js"));
    }

    #[test]
    fn windows_command_text_names_the_program() {
        assert_eq!(
            runs(
                "cmd.exe",
                &["cmd", "/d", "/c", "C:\\npm\\codex.cmd --model x"]
            )
            .as_deref(),
            Some("codex.cmd")
        );
        assert_eq!(
            runs(
                "pwsh.exe",
                &["pwsh", "-NoProfile", "-File", "C:\\tools\\build.ps1"]
            )
            .as_deref(),
            Some("build.ps1")
        );
        assert_eq!(
            runs("powershell.exe", &["powershell", "-enc", "ZQBjAGgAbwA="]),
            None
        );
    }

    #[test]
    fn a_windows_program_path_may_hold_spaces() {
        // Split into argv, quotes gone.
        assert_eq!(
            runs(
                "cmd.exe",
                &[
                    "cmd",
                    "/c",
                    "C:\\Program Files\\nodejs\\npm.cmd",
                    "run",
                    "dev"
                ]
            )
            .as_deref(),
            Some("npm.cmd")
        );
        // One command string, the path quoted.
        assert_eq!(
            runs(
                "cmd.exe",
                &[
                    "cmd",
                    "/c",
                    "\"C:\\Program Files\\nodejs\\npm.cmd\" run dev"
                ]
            )
            .as_deref(),
            Some("npm.cmd")
        );
        assert_eq!(
            runs(
                "pwsh.exe",
                &[
                    "pwsh",
                    "-Command",
                    "& 'C:\\Program Files\\Tools\\tool.exe' -a"
                ]
            )
            .as_deref(),
            Some("tool.exe")
        );
        // A path later in the text is an argument, not the program.
        assert_eq!(
            runs("cmd.exe", &["cmd", "/c", "git diff C:\\a b\\c.exe"]).as_deref(),
            Some("git")
        );
    }

    fn program(executable: &str) -> Option<ForegroundProgram> {
        Some(ForegroundProgram {
            executable: executable.to_string(),
            runs: None,
        })
    }

    #[test]
    fn a_first_look_publishes_at_once() {
        let mut record = Record::default();
        assert!(observe(&mut record, program("zsh"), Instant::now()));
        assert_eq!(record.published, program("zsh"));
    }

    #[test]
    fn a_change_is_published_only_once_it_holds() {
        let start = Instant::now();
        let mut record = Record::default();
        observe(&mut record, program("zsh"), start);
        assert!(!observe(
            &mut record,
            program("vim"),
            start + Duration::from_millis(750)
        ));
        assert_eq!(record.published, program("zsh"));
        assert!(!observe(
            &mut record,
            program("vim"),
            start + Duration::from_millis(900)
        ));
        assert!(observe(
            &mut record,
            program("vim"),
            start + Duration::from_millis(1300)
        ));
        assert_eq!(record.published, program("vim"));
    }

    #[test]
    fn a_command_that_ends_in_time_never_shows() {
        let start = Instant::now();
        let mut record = Record::default();
        observe(&mut record, program("zsh"), start);
        observe(
            &mut record,
            program("ls"),
            start + Duration::from_millis(750),
        );
        assert!(!observe(
            &mut record,
            program("zsh"),
            start + Duration::from_millis(1300)
        ));
        assert_eq!(record.published, program("zsh"));
        assert!(record.candidate.is_none());
    }

    #[test]
    fn the_look_owed_after_activity_reads_afresh() {
        // Prompted by activity: the cache will do.
        assert!(!needs_fresh_look(true, false, true));
        // Owed after it: whatever the burst left must be seen.
        assert!(needs_fresh_look(true, false, false));
        // A first look and a confirmation always read afresh.
        assert!(needs_fresh_look(false, false, true));
        assert!(needs_fresh_look(true, true, true));
    }
}
