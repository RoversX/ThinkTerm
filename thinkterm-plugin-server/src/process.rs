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

use crate::stamp::Stamp;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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
const STACK: usize = 256 * 1024;

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
    pub dir: &'a Path,
    pub data_dir: &'a Path,
    pub ready_within: Duration,
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
    /// The program file, and how it was when started: a program rebuilt
    /// since is started again.
    pub program: PathBuf,
    pub program_stamp: Option<Stamp>,
    listener: Arc<dyn Listener>,
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
        program_stamp: Option<Stamp>,
        listener: Arc<dyn Listener>,
    ) -> Result<Self, String> {
        create_private_dir(start.data_dir)
            .map_err(|err| format!("cannot make {}: {err}", start.data_dir.display()))?;
        let mut command = Command::new(start.program);
        command
            .args(start.args)
            .current_dir(start.dir)
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
                        match serde_json::from_slice::<FromPlugin>(&line) {
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
            std::thread::Builder::new()
                .name(format!("plugin-{id}-err"))
                .stack_size(STACK)
                .spawn(move || {
                    log_errors(errors, &ERRORS_LEFT, |line| {
                        log::info!("plugin {id}: {line}")
                    });
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
            program: start.program.to_path_buf(),
            program_stamp,
            listener,
        })
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

    /// The client waiting for the answer to `id`, now that it came.
    pub fn answered(&mut self, id: u64) -> Option<Waiter> {
        self.pending.remove(&id).map(|(waiter, _)| waiter)
    }

    /// Lets go of the calls made longer than [`PENDING_EXPIRY`] ago: whoever
    /// made them stopped waiting long since. Hands them back.
    pub fn expire(&mut self) -> Vec<Waiter> {
        let mut expired = Vec::new();
        self.pending.retain(|_, (waiter, made)| {
            let keep = made.elapsed() < PENDING_EXPIRY;
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

/// Hands what a program writes to its standard error to `log`, a line at a
/// time, while `left` allows. Past that the rest is read and dropped, said
/// once: a program is never held up writing it.
fn log_errors(from: impl Read, left: &AtomicUsize, mut log: impl FnMut(&str)) {
    let mut from = BufReader::new(from);
    let mut line = Vec::new();
    let mut dropping = false;
    loop {
        line.clear();
        match Read::take(&mut from, ERROR_LINE).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let allowed = left
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(line.len())
            })
            .is_ok();
        if allowed {
            let text = String::from_utf8_lossy(&line);
            let text = text.trim_end();
            if !text.is_empty() {
                log(text);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_error_is_logged_by_the_line_within_its_allowance() {
        let written = b"first\n\nsecond line\nthird\nfourth\n".to_vec();
        // Room for the first three lines, the empty one among them.
        let left = AtomicUsize::new(6 + 1 + 12);
        let mut logged = Vec::new();
        log_errors(&written[..], &left, |line| logged.push(line.to_string()));
        assert_eq!(logged.len(), 3, "{logged:?}");
        assert_eq!(logged[..2], ["first", "second line"]);
        assert!(logged[2].contains("dropped"), "{logged:?}");
    }

    #[test]
    fn a_line_without_end_is_logged_in_pieces() {
        let written = vec![b'x'; ERROR_LINE as usize + 10];
        let left = AtomicUsize::new(usize::MAX);
        let mut logged = Vec::new();
        log_errors(&written[..], &left, |line| logged.push(line.len()));
        assert_eq!(logged, [ERROR_LINE as usize, 10]);
    }
}
