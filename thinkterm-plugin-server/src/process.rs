//! An installed plugin's program while it runs: the threads that carry its
//! input and output, the calls it has not answered, and how it is stopped.
//!
//! Its input is written from a queue of its own, like a client's: a program
//! that stops reading fills the queue, not the host's lock. What it says is
//! read on a thread that hands every message to the host, and that reaps
//! the program once its output ends.
//!
//! A program told to stop is [`Exiting`] until it is gone, and the host
//! starts no other run of the plugin meanwhile: one writing its files on
//! the way out would write over what a new run had already saved.
//!
//! What a program writes to its standard error goes to the host's log, and
//! the last of it is kept in the plugin's [`Log`] as well: for saying why a
//! run stopped -- the last thing it said, read to its end first -- and for
//! `thinkterm plugin logs`.

use crate::manifest::Source;
use crate::stamp::Stamp;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thinkterm_plugin_sdk::protocol::{self, FromPlugin, ToPlugin, API};

/// Messages that may wait for one program. A program this far behind is
/// not reading.
const QUEUE: usize = 256;
/// Calls one program may have unanswered.
pub const PENDING_LIMIT: usize = 64;
/// How long a call waits for its answer here. Longer than any client waits
/// (`CALL_TIMEOUT`), so a call is only let go once whoever made it has given
/// up: a plugin that never answers does not keep its slots.
pub const PENDING_EXPIRY: Duration = Duration::from_secs(70);
/// How long a program has to exit once asked to, or once its output ends.
pub const STOP_GRACE: Duration = Duration::from_secs(2);
/// Lines that are not messages logged for one program; a program that
/// prints its debugging to standard output would fill the log otherwise.
const NOISE_LOGGED: usize = 5;
/// What the programs' standard error may put in the host's log between
/// them while it runs. Past it, what they write is dropped: a program
/// printing in a loop does not fill the disk. The log is cut back when the
/// host next starts.
const ERRORS_LOGGED: usize = 4 * 1024 * 1024;
/// The longest stretch of standard error logged as one line.
const ERROR_LINE: u64 = 4096;
/// What is left of [`ERRORS_LOGGED`].
static ERRORS_LEFT: AtomicUsize = AtomicUsize::new(ERRORS_LOGGED);
/// The lines of standard error a plugin's [`Log`] keeps, across its runs,
/// and the most bytes they come to.
const LOG_LINES: usize = 400;
const LOG_BYTES: usize = 64 * 1024;
/// How long the host waits, once a program has exited, for the rest of
/// what it wrote to standard error: something it started may keep that
/// open after it is gone.
const LAST_WORDS_WITHIN: Duration = Duration::from_millis(300);
/// The longest last thing said that a reason quotes.
const LAST_WORDS_LIMIT: usize = 300;
/// The stack of the host's threads, here and in `host`. They parse JSON and
/// move bytes, and any of them may read a manifest -- a scan, a tidy -- one
/// nested as deep as the TOML parser goes (80 levels) taking 512 KiB in a
/// release build and 2 MiB in a debug one, unoptimized: twice that here. A
/// thread that runs out aborts the host; what it does not touch of its
/// stack is only reserved.
pub const STACK: usize = if cfg!(debug_assertions) {
    4 * 1024 * 1024
} else {
    1024 * 1024
};

/// What the host hears about a program, on the program's own threads.
pub trait Listener: Send + Sync + 'static {
    /// It said something.
    fn said(&self, id: &str, generation: u64, message: FromPlugin);
    /// It has exited, for `why`: its output ended, or it was told to stop
    /// and is gone. Told once or twice for one run; the second time finds
    /// nothing left to do.
    fn ended(&self, id: &str, generation: u64, why: String);
    /// Its time to say it is ready is up.
    fn late(&self, id: &str, generation: u64);
}

/// Who is waiting for an answer: a client, and its own id for the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Waiter {
    pub client: u64,
    pub id: u64,
}

/// What starting a program takes.
pub struct Start<'a> {
    pub id: &'a str,
    /// Tells this run of the program from any other.
    pub generation: u64,
    pub program: &'a Path,
    pub args: &'a [String],
    /// What it is given in its environment besides what every plugin is.
    pub env: &'a [(String, String)],
    /// What of the host's environment it is not given.
    pub unset: &'a [String],
    pub dir: &'a Path,
    pub data_dir: &'a Path,
    pub ready_within: Duration,
    /// Where what it writes to standard error is kept.
    pub log: &'a SharedLog,
}

pub struct Process {
    id: String,
    pub generation: u64,
    child: Arc<Mutex<Child>>,
    outbox: SyncSender<Vec<u8>>,
    /// The calls it has not answered, and when each was made.
    pending: HashMap<u64, (Waiter, Instant)>,
    next_id: u64,
    /// It said it is ready.
    pub ready: bool,
    /// What it is made of, and how that was when it started: a program
    /// rebuilt since, or a script edited, is started again.
    pub source: Source,
    pub source_stamp: Option<Stamp>,
    listener: Arc<dyn Listener>,
}

/// What a plugin's programs wrote to standard error, as [`Log`] keeps it,
/// with word of each run's standard error ending.
pub type SharedLog = Arc<(Mutex<Log>, Condvar)>;

pub fn new_log() -> SharedLog {
    Arc::new((Mutex::new(Log::default()), Condvar::new()))
}

/// The last of what a plugin's programs wrote to standard error, across
/// its runs: at most [`LOG_LINES`] lines and [`LOG_BYTES`], each run's
/// begun by a line saying when it started. Terminal controls are taken out.
#[derive(Debug, Default)]
pub struct Log {
    /// The run each line is of, whether it is the line saying when that
    /// run started, and the line.
    lines: VecDeque<(u64, bool, String)>,
    bytes: usize,
    cut: bool,
    /// The last runs whose standard error has ended: all they wrote is in.
    ended: VecDeque<u64>,
}

impl Log {
    fn begin(&mut self, generation: u64) {
        let line = format!("-- started at {} --", clock_now());
        self.add(generation, true, line);
    }

    fn push(&mut self, generation: u64, line: String) {
        self.add(generation, false, line);
    }

    fn add(&mut self, generation: u64, begins: bool, line: String) {
        self.bytes += line.len();
        self.lines.push_back((generation, begins, line));
        while self.lines.len() > LOG_LINES || (self.bytes > LOG_BYTES && self.lines.len() > 1) {
            if let Some((_, _, gone)) = self.lines.pop_front() {
                self.bytes -= gone.len();
                self.cut = true;
            }
        }
    }

    fn end(&mut self, generation: u64) {
        self.ended.push_back(generation);
        while self.ended.len() > 8 {
            self.ended.pop_front();
        }
    }

    /// Every line kept, oldest first, and whether older ones were let go.
    pub fn lines(&self) -> (Vec<String>, bool) {
        (
            self.lines.iter().map(|(_, _, line)| line.clone()).collect(),
            self.cut,
        )
    }

    /// The last thing run `generation` said that tells why it stopped, on
    /// one line: for a Python traceback, its error and where in the
    /// plugin's own files it was raised.
    pub fn last_words(&self, generation: u64) -> Option<String> {
        let said: Vec<&str> = self
            .lines
            .iter()
            .filter(|(run, begins, _)| *run == generation && !begins)
            .map(|(_, _, line)| line.as_str())
            .collect();
        last_words(&said)
    }
}

/// `why` a run ended, with the last thing it said after it when it said
/// something.
pub fn with_last_words(why: String, words: Option<String>) -> String {
    match words {
        Some(words) if !why.contains(&words) => format!("{why}: {words}"),
        _ => why,
    }
}

/// Waits a moment for run `generation`'s standard error to have ended,
/// then says the last thing it said.
pub fn last_words_of(log: &SharedLog, generation: u64) -> Option<String> {
    let (log, ended) = &**log;
    let deadline = Instant::now() + LAST_WORDS_WITHIN;
    let mut held = log.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    while !held.ended.contains(&generation) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        held = match ended.wait_timeout(held, left) {
            Ok((held, _)) => held,
            Err(poisoned) => poisoned.into_inner().0,
        };
    }
    held.last_words(generation)
}

fn last_words(said: &[&str]) -> Option<String> {
    let said: Vec<&str> = said
        .iter()
        .map(|line| line.trim_end())
        .filter(|line| !line.trim().is_empty())
        .collect();
    let last = *said.last()?;
    let words = match said
        .iter()
        .rposition(|line| line.starts_with("Traceback (most recent call last):"))
    {
        Some(start) => python_error(&said[start..]).unwrap_or_else(|| last.to_string()),
        None => last.to_string(),
    };
    Some(shorten(&words, LAST_WORDS_LIMIT))
}

/// A Python traceback's error, and the innermost place it went through in
/// a file of the plugin's own, not the helper ThinkTerm gives scripts --
/// `NameError: name 'x' is not defined (main.py, line 8)`.
fn python_error(traceback: &[&str]) -> Option<String> {
    let error = traceback
        .iter()
        .skip(1)
        .rev()
        .find(|line| !line.starts_with(' ') && !line.starts_with('\t'))?;
    let places: Vec<(String, String)> = traceback
        .iter()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("File \"")?;
            let (file, rest) = rest.split_once('"')?;
            let line = rest.strip_prefix(", line ")?;
            let line: String = line.chars().take_while(char::is_ascii_digit).collect();
            let file = file.rsplit(['/', '\\']).next().unwrap_or(file).to_string();
            Some((file, line))
        })
        .collect();
    let place = places
        .iter()
        .rev()
        .find(|(file, _)| file != "thinkterm.py" && !file.starts_with('<'))
        .or_else(|| places.last());
    Some(match place {
        Some((file, line)) => format!("{error} ({file}, line {line})"),
        None => error.to_string(),
    })
}

fn shorten(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut short: String = text.chars().take(limit.saturating_sub(1)).collect();
    short.push('…');
    short
}

/// `text` without what a terminal would act on: escape sequences, and
/// control characters but tabs.
fn plain(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters, then one final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC and the other strings: up to BEL or ST.
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => plain.push(c),
            c if c.is_control() => {}
            c => plain.push(c),
        }
    }
    plain
}

/// The time of day now, in UTC, as a log line gives it: 13:04:59 UTC.
fn clock_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let day = secs % 86_400;
    format!(
        "{:02}:{:02}:{:02} UTC",
        day / 3600,
        day % 3600 / 60,
        day % 60
    )
}

/// A program told to stop, until it is gone.
pub struct Exiting {
    /// The id it ran as, which names its data directory.
    pub id: String,
    pub generation: u64,
    child: Arc<Mutex<Child>>,
}

impl Exiting {
    /// Whether it has exited, whatever has or has not been told of it.
    pub fn gone(&self) -> bool {
        exited(&self.child)
    }
}

/// Why a call was not handed to the program.
pub enum Refused {
    /// Too many of its calls are unanswered.
    Busy(String),
    /// It is not reading what it is sent, or its input is closed: it is not
    /// going to answer anything.
    Gone(String),
}

impl Process {
    pub fn start(
        start: Start<'_>,
        source: Source,
        source_stamp: Option<Stamp>,
        listener: Arc<dyn Listener>,
    ) -> Result<Self, String> {
        create_private_dir(start.data_dir)
            .map_err(|err| format!("cannot make {}: {err}", start.data_dir.display()))?;
        let mut command = Command::new(start.program);
        command
            .args(start.args)
            .current_dir(start.dir)
            .envs(start.env.iter().map(|(name, value)| (name, value)))
            .env("THINKTERM_PLUGIN_ID", start.id)
            .env("THINKTERM_PLUGIN_DIR", start.dir)
            .env("THINKTERM_PLUGIN_DATA", start.data_dir)
            .env("THINKTERM_PLUGIN_API", API.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Into the host's log, a line at a time and within an allowance.
            .stderr(Stdio::piped())
            // Which pane and which mux its `thinkterm cli` reaches is for the
            // context to say, not for whoever happened to start the host:
            // without these it finds the desktop by itself, and the mux
            // server with `--prefer-mux`.
            .env_remove("WEZTERM_PANE")
            .env_remove("WEZTERM_UNIX_SOCKET");
        for name in start.unset {
            command.env_remove(name);
        }
        // The command line shipped beside the host, which reaches the
        // terminal beside a panel (`Env::terminal`): a plugin need not
        // find it on a PATH ThinkTerm was started with.
        if let Some(cli) = cli_program() {
            command.env("THINKTERM_CLI", cli);
        }
        #[cfg(windows)]
        {
            // The host has no console; a console program would open one.
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .map_err(|err| format!("cannot start {}: {err}", start.program.display()))?;
        log::info!(
            "started plugin {} ({}), pid {}",
            start.id,
            start.program.display(),
            child.id()
        );
        let (Some(mut input), Some(output), Some(errors)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("its input and output were not piped".into());
        };
        let child = Arc::new(Mutex::new(child));
        let log = Arc::clone(start.log);
        log.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .begin(start.generation);
        let (outbox, queued) = sync_channel::<Vec<u8>>(QUEUE);

        let writing = std::thread::Builder::new()
            .name(format!("plugin-{}-in", start.id))
            .stack_size(STACK)
            .spawn(move || {
                for line in queued {
                    if input.write_all(&line).and_then(|()| input.flush()).is_err() {
                        break;
                    }
                }
                // Dropping the input closes it: the program's cue to exit.
            });

        let reading = {
            let id = start.id.to_string();
            let generation = start.generation;
            let child = Arc::clone(&child);
            let listener = Arc::clone(&listener);
            let log = Arc::clone(&log);
            std::thread::Builder::new()
                .name(format!("plugin-{id}-out"))
                .stack_size(STACK)
                .spawn(move || {
                    let mut output = BufReader::new(output);
                    let mut line = Vec::new();
                    let mut noise = 0;
                    let why = loop {
                        match protocol::read_line(&mut output, &mut line) {
                            Ok(true) => {}
                            Ok(false) => break None,
                            Err(err) => break Some(format!("{err}")),
                        }
                        match FromPlugin::read(&line) {
                            Ok(message) => listener.said(&id, generation, message),
                            Err(err) if noise < NOISE_LOGGED => {
                                noise += 1;
                                log::warn!("plugin {id} wrote a line that is not a message: {err}");
                            }
                            Err(_) => {}
                        }
                    };
                    let exit = reap(&child);
                    let why = match why {
                        Some(broken) => format!("{broken}; {exit}"),
                        None => exit,
                    };
                    // What it said last is most often why it stopped.
                    let why = with_last_words(why, last_words_of(&log, generation));
                    listener.ended(&id, generation, why);
                })
        };

        let watching = {
            let id = start.id.to_string();
            let generation = start.generation;
            let ready_within = start.ready_within;
            let listener = Arc::clone(&listener);
            std::thread::Builder::new()
                .name(format!("plugin-{id}-start"))
                .stack_size(STACK)
                .spawn(move || {
                    std::thread::sleep(ready_within);
                    listener.late(&id, generation);
                })
        };

        let logging = {
            let id = start.id.to_string();
            let generation = start.generation;
            let log = Arc::clone(&log);
            std::thread::Builder::new()
                .name(format!("plugin-{id}-err"))
                .stack_size(STACK)
                .spawn(move || {
                    let (kept, ended) = &*log;
                    let keep = |line: &str| {
                        kept.lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .push(generation, line.to_string());
                    };
                    log_errors(errors, &ERRORS_LEFT, keep, |line| {
                        log::info!("plugin {id}: {line}")
                    });
                    kept.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .end(generation);
                    ended.notify_all();
                })
        };

        if let Err(err) = writing.and(reading).and(watching).and(logging) {
            kill(&child);
            return Err(format!("cannot start the threads for it: {err}"));
        }
        Ok(Self {
            id: start.id.to_string(),
            generation: start.generation,
            child,
            outbox,
            pending: HashMap::new(),
            next_id: 0,
            ready: false,
            source,
            source_stamp,
            listener,
        })
    }

    /// Whether what it is made of has changed since it started: it is to
    /// be started again.
    pub fn changed(&self) -> bool {
        self.source.stamp() != self.source_stamp
    }

    /// Sends a call or a command, made by `message` from the id it is
    /// given; `waiter` is answered when the program answers it.
    pub fn ask(
        &mut self,
        waiter: Waiter,
        message: impl FnOnce(u64) -> ToPlugin,
    ) -> Result<(), Refused> {
        if self.pending.len() >= PENDING_LIMIT {
            return Err(Refused::Busy(format!(
                "it has {PENDING_LIMIT} calls unanswered already"
            )));
        }
        self.next_id += 1;
        let mut line = Vec::new();
        protocol::write_message(&mut line, &message(self.next_id))
            .map_err(|err| Refused::Busy(format!("{err}")))?;
        match self.outbox.try_send(line) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                return Err(Refused::Gone("it is not reading what it is sent".into()))
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(Refused::Gone("its input is closed".into()))
            }
        }
        self.pending.insert(self.next_id, (waiter, Instant::now()));
        Ok(())
    }

    /// Sends a message no answer is waited for: one about a panel.
    pub fn tell(&mut self, message: &ToPlugin) -> Result<(), Refused> {
        let mut line = Vec::new();
        protocol::write_message(&mut line, message)
            .map_err(|err| Refused::Busy(format!("{err}")))?;
        self.tell_line(line)
    }

    /// Sends a message already written out as a line, its end included.
    pub fn tell_line(&mut self, line: Vec<u8>) -> Result<(), Refused> {
        match self.outbox.try_send(line) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                Err(Refused::Gone("it is not reading what it is sent".into()))
            }
            Err(TrySendError::Disconnected(_)) => Err(Refused::Gone("its input is closed".into())),
        }
    }

    /// Whether calls to it wait for their answers.
    pub fn busy(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The client waiting for the answer to `id`, now that it came.
    pub fn answered(&mut self, id: u64) -> Option<Waiter> {
        self.pending.remove(&id).map(|(waiter, _)| waiter)
    }

    /// When the oldest call still waiting for its answer was made.
    pub fn oldest_call(&self) -> Option<Instant> {
        self.pending.values().map(|(_, made)| *made).min()
    }

    /// Lets go of the calls made longer than `limit` ago -- normally
    /// [`PENDING_EXPIRY`]: whoever made them stopped waiting long since.
    /// Hands them back.
    pub fn expire(&mut self, limit: Duration) -> Vec<Waiter> {
        let mut expired = Vec::new();
        self.pending.retain(|_, (waiter, made)| {
            let keep = made.elapsed() < limit;
            if !keep {
                expired.push(*waiter);
            }
            keep
        });
        expired
    }

    /// Lets go of the calls `client` made: it has gone, and nobody is left
    /// to answer. The program may still answer them, into nothing.
    pub fn forget_client(&mut self, client: u64) {
        self.pending
            .retain(|_, (waiter, _)| waiter.client != client);
    }

    /// Asks the program to exit, and kills it if it has not within
    /// [`STOP_GRACE`]; the host is told once it is gone. Hands back the
    /// calls it will not answer, and the program until it is gone.
    pub fn stop(self) -> (Vec<Waiter>, Exiting) {
        let Self {
            id,
            generation,
            child,
            outbox,
            pending,
            listener,
            ..
        } = self;
        let mut line = Vec::new();
        if protocol::write_message(&mut line, &ToPlugin::Stop).is_ok() {
            let _ = outbox.try_send(line);
        }
        // The writer closes the program's input once the queue is done.
        drop(outbox);
        let exiting = Exiting {
            id: id.clone(),
            generation,
            child: Arc::clone(&child),
        };
        let stopper = std::thread::Builder::new()
            .name("plugin-stop".into())
            .stack_size(STACK)
            .spawn({
                let child = Arc::clone(&child);
                move || {
                    let why = reap(&child);
                    listener.ended(&id, generation, why);
                }
            });
        if stopper.is_err() {
            // Gone at once, then; its reader tells the host.
            kill(&child);
        }
        let waiters = pending.into_values().map(|(waiter, _)| waiter).collect();
        (waiters, exiting)
    }

    /// Kills the program at once: it failed to start, or stopped reading.
    /// Hands back the calls it will not answer.
    pub fn kill(self) -> Vec<Waiter> {
        kill(&self.child);
        self.pending
            .into_values()
            .map(|(waiter, _)| waiter)
            .collect()
    }
}

/// Stops every program at once, for the host to exit: asks each running
/// one to exit, gives them and the ones already exiting `grace` between
/// them, and kills whatever is left.
pub fn stop_all(processes: Vec<Process>, exiting: Vec<Exiting>, grace: Duration) {
    let mut running: Vec<Arc<Mutex<Child>>> = processes
        .into_iter()
        .map(|process| {
            let Process { child, outbox, .. } = process;
            let mut line = Vec::new();
            if protocol::write_message(&mut line, &ToPlugin::Stop).is_ok() {
                let _ = outbox.try_send(line);
            }
            // Closing the queue closes the program's input once it is sent.
            drop(outbox);
            child
        })
        .chain(exiting.into_iter().map(|exiting| exiting.child))
        .collect();
    let deadline = Instant::now() + grace;
    while !running.is_empty() && Instant::now() < deadline {
        running.retain(|child| !exited(child));
        std::thread::sleep(Duration::from_millis(10));
    }
    for child in &running {
        kill(child);
    }
}

/// Whether the program has exited, reaping it if so. Never waits: nothing
/// holds a program's lock while it dies.
fn exited(child: &Mutex<Child>) -> bool {
    child
        .lock()
        .map(|mut child| !matches!(child.try_wait(), Ok(None)))
        .unwrap_or(true)
}

/// Waits for the program to exit, killing it if it has not within
/// [`STOP_GRACE`], and says how it ended -- once it is gone, so that no
/// other run starts while it can still write.
fn reap(child: &Mutex<Child>) -> String {
    let deadline = Instant::now() + STOP_GRACE;
    loop {
        let waited = child.lock().map(|mut child| child.try_wait());
        match waited {
            Ok(Ok(Some(status))) => return describe(status),
            Ok(Ok(None)) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(Ok(None)) => {
                kill(child);
                // Stuck in the system, it dies when the system lets it.
                while !exited(child) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                return "it did not exit, and was killed".into();
            }
            Ok(Err(err)) => return format!("waiting for it: {err}"),
            Err(_) => return "it ended".into(),
        }
    }
}

/// Kills the program without waiting for it: the caller may hold the
/// host's lock. Whoever reads its output reaps it.
fn kill(child: &Mutex<Child>) {
    if let Ok(mut child) = child.lock() {
        let _ = child.kill();
    }
}

/// Hands what a program writes to its standard error to `keep`, a line at
/// a time without terminal controls, and to `log` while `left` allows. Past
/// that the rest goes to `keep` alone, which said once: a program is never
/// held up writing it.
fn log_errors(
    from: impl Read,
    left: &AtomicUsize,
    mut keep: impl FnMut(&str),
    mut log: impl FnMut(&str),
) {
    let mut from = BufReader::new(from);
    let mut line = Vec::new();
    let mut dropping = false;
    loop {
        line.clear();
        match Read::take(&mut from, ERROR_LINE).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = plain(String::from_utf8_lossy(&line).trim_end());
        if !text.trim().is_empty() {
            keep(&text);
        }
        let allowed = left
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(line.len())
            })
            .is_ok();
        if allowed {
            if !text.is_empty() {
                log(&text);
            }
        } else if !dropping {
            dropping = true;
            log(&format!(
                "plugins wrote {ERRORS_LOGGED} bytes to standard error; \
                 the rest is dropped until the plugin server starts again"
            ));
        }
    }
}

fn describe(status: ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("it exited with status {code}"),
        None => format!("it ended: {status}"),
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// The `thinkterm` program beside this one, whose `cli` a plugin runs.
fn cli_program() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "thinkterm.exe"
    } else {
        "thinkterm"
    };
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    Some(exe.with_file_name(name)).filter(|cli| cli.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_error_is_logged_by_the_line_within_its_allowance() {
        let written = b"first\n\nsecond line\nthird\nfourth\n".to_vec();
        // Room for the first three lines, the empty one among them.
        let left = AtomicUsize::new(6 + 1 + 12);
        let mut logged = Vec::new();
        let mut kept = Vec::new();
        log_errors(
            &written[..],
            &left,
            |line| kept.push(line.to_string()),
            |line| logged.push(line.to_string()),
        );
        assert_eq!(logged.len(), 3, "{logged:?}");
        assert_eq!(logged[..2], ["first", "second line"]);
        assert!(logged[2].contains("dropped"), "{logged:?}");
        assert_eq!(
            kept,
            ["first", "second line", "third", "fourth"],
            "the plugin's own log keeps what the host's has no room for"
        );
    }

    #[test]
    fn a_line_without_end_is_logged_in_pieces() {
        let written = vec![b'x'; ERROR_LINE as usize + 10];
        let left = AtomicUsize::new(usize::MAX);
        let mut logged = Vec::new();
        log_errors(&written[..], &left, |_| {}, |line| logged.push(line.len()));
        assert_eq!(logged, [ERROR_LINE as usize, 10]);
    }

    #[test]
    fn what_a_terminal_would_act_on_is_taken_out() {
        assert_eq!(plain("\u{1b}[31mred\u{1b}[0m text"), "red text");
        assert_eq!(plain("a\u{1b}]0;title\u{7}b"), "ab");
        assert_eq!(
            plain("a\u{1b}]8;;http://x\u{1b}\\link\u{1b}]8;;\u{1b}\\b"),
            "alinkb"
        );
        assert_eq!(plain("tab\tkept\r\u{8}"), "tab\tkept");
        assert_eq!(plain("中文 ok"), "中文 ok");
    }

    #[test]
    fn a_python_traceback_comes_to_its_error_and_where_in_the_plugin() {
        let said = [
            "Traceback (most recent call last):",
            "  File \"main.py\", line 40, in <module>",
            "    thinkterm.run(Hello())",
            "  File \"/home/u/plugins/hello/thinkterm.py\", line 512, in run",
            "    plugin.draw(ui)",
            "  File \"main.py\", line 12, in draw",
            "    ui.text(f\"{count}\")",
            "            ^^^^^",
            "NameError: name 'count' is not defined",
        ];
        assert_eq!(
            last_words(&said).as_deref(),
            Some("NameError: name 'count' is not defined (main.py, line 12)")
        );
        // Raised in the helper itself, with nothing of the plugin's after.
        let helper = [
            "Traceback (most recent call last):",
            "  File \"C:\\p\\thinkterm.py\", line 9, in run",
            "ValueError: bad",
        ];
        assert_eq!(
            last_words(&helper).as_deref(),
            Some("ValueError: bad (thinkterm.py, line 9)")
        );
        assert_eq!(
            last_words(&["starting", "error: no config", ""]).as_deref(),
            Some("error: no config")
        );
        assert_eq!(last_words(&["  ", ""]), None);
        let long = "x".repeat(1000);
        assert_eq!(
            last_words(&[long.as_str()]).unwrap().chars().count(),
            LAST_WORDS_LIMIT
        );
    }

    #[test]
    fn a_log_keeps_the_last_of_every_run_and_each_runs_last_words() {
        let mut log = Log::default();
        log.begin(1);
        log.push(1, "first run".into());
        log.begin(2);
        log.push(2, "second run".into());
        assert_eq!(log.last_words(1).as_deref(), Some("first run"));
        assert_eq!(log.last_words(2).as_deref(), Some("second run"));
        assert_eq!(log.last_words(3), None);
        let (lines, cut) = log.lines();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("-- started at ") && lines[0].ends_with(" UTC --"));
        assert!(!cut);
        for n in 0..LOG_LINES {
            log.push(2, format!("line {n}"));
        }
        let (lines, cut) = log.lines();
        assert_eq!(lines.len(), LOG_LINES);
        assert!(cut);
        assert_eq!(lines.last().unwrap(), &format!("line {}", LOG_LINES - 1));
        log.push(2, "y".repeat(LOG_BYTES));
        assert_eq!(log.lines().0.len(), 1, "one line too long for the rest");
    }

    #[test]
    fn a_runs_last_words_wait_for_its_standard_error_to_end() {
        let log = new_log();
        log.0.lock().unwrap().push(5, "early".into());
        let later = Arc::clone(&log);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let mut held = later.0.lock().unwrap();
            held.push(5, "last".into());
            held.end(5);
            later.1.notify_all();
        });
        assert_eq!(last_words_of(&log, 5).as_deref(), Some("last"));
        writer.join().unwrap();
        // One whose standard error never ends is not waited for long.
        let started = Instant::now();
        assert_eq!(last_words_of(&log, 6), None);
        assert!(started.elapsed() < LAST_WORDS_WITHIN * 3);
        assert_eq!(
            with_last_words("it exited with status 1".into(), Some("boom".into())),
            "it exited with status 1: boom"
        );
        assert_eq!(
            with_last_words("x: boom".into(), Some("boom".into())),
            "x: boom"
        );
        assert_eq!(with_last_words("quiet".into(), None), "quiet");
    }
}
