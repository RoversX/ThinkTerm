//! Reaching the host: connecting to it, starting it when nothing answers,
//! and replacing one that speaks an older protocol.

use crate::wire::{read_frame, write_frame, FromHost, PanelEvent, PanelRequest, ToHost, PROTOCOL};
use anyhow::{anyhow, bail, Context};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use wezterm_uds::UnixStream;

/// How long a start may take before the client gives up on it. Generous:
/// the first run of a freshly installed program can wait on the system's
/// checks of it.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// Between attempts to reach a host that is starting or quitting.
const RETRY: Duration = Duration::from_millis(20);
/// A host that accepted a connection says hello at once; one that does not
/// within this is not a host to use.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// The log is the host's standard error, and it says little; one that grew
/// past this anyway starts again empty.
const LOG_LIMIT: u64 = 1024 * 1024;

/// Everything a client needs to find the host, or start it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub socket: PathBuf,
    pub lock: PathBuf,
    pub data_dir: PathBuf,
    pub log: PathBuf,
    pub program: PathBuf,
}

enum Reached {
    Ready(Connection),
    /// A host from an older build: it is asked to quit.
    Older(Connection),
    Newer(u32),
}

impl Host {
    /// The host this build ships, where every client on this machine looks
    /// for it.
    pub fn for_this_build() -> io::Result<Self> {
        Ok(Self {
            socket: crate::paths::socket(),
            lock: crate::paths::lock(),
            data_dir: crate::paths::data_dir(),
            log: crate::paths::log(),
            program: crate::paths::host_program()?,
        })
    }

    /// A connection to the host, which is started first when nothing
    /// answers. Blocking for as long as a start takes: never call it on a
    /// thread that paints.
    pub fn connect(&self) -> anyhow::Result<Connection> {
        let deadline = Instant::now() + START_TIMEOUT;
        let mut started = false;
        loop {
            match self.reach() {
                Ok(Reached::Ready(connection)) => return Ok(connection),
                Ok(Reached::Older(mut connection)) => {
                    // It removes its socket before it exits; until then the
                    // next attempt finds it still there and asks again.
                    log::info!("replacing a plugin host from an older build");
                    let _ = connection.send(&ToHost::Quit);
                }
                Ok(Reached::Newer(protocol)) => bail!(
                    "a newer ThinkTerm's plugin host is running (protocol {protocol}, \
                     this build speaks {PROTOCOL})"
                ),
                Err(err) if nobody_listens(&err) => {
                    if !started {
                        self.start()?;
                        started = true;
                    }
                }
                // Accepted and closed: a host on its way out.
                Err(err) if is_transient(&err) => {}
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("connect to {}", self.socket.display()))
                }
            }
            if Instant::now() >= deadline {
                bail!(
                    "the plugin host did not answer at {} within {START_TIMEOUT:?}",
                    self.socket.display()
                );
            }
            std::thread::sleep(RETRY);
        }
    }

    fn reach(&self) -> io::Result<Reached> {
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(HELLO_TIMEOUT))?;
        let hello = read_frame(&mut stream)?;
        stream.set_read_timeout(None)?;
        let connection = Connection { stream };
        match FromHost::decode(&hello) {
            Ok(FromHost::Hello { protocol }) if protocol == PROTOCOL => {
                Ok(Reached::Ready(connection))
            }
            Ok(FromHost::Hello { protocol }) if protocol < PROTOCOL => {
                Ok(Reached::Older(connection))
            }
            Ok(FromHost::Hello { protocol }) => Ok(Reached::Newer(protocol)),
            _ => Err(io::Error::new(
                ErrorKind::InvalidData,
                "the plugin host did not say hello",
            )),
        }
    }

    /// Starts the host in the background. It holds the lock while it runs,
    /// so a start that races another's gives way to it.
    fn start(&self) -> anyhow::Result<()> {
        if let Some(dir) = self.socket.parent() {
            create_private_dir(dir)?;
        }
        let log = open_log(&self.log)?;
        let mut command = Command::new(&self.program);
        command
            .arg("--socket")
            .arg(&self.socket)
            .arg("--lock")
            .arg(&self.lock)
            .arg("--data-dir")
            .arg(&self.data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        // Its own process group: a Ctrl-C meant for the terminal that
        // started a client is not meant for the host.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .map_err(|err| anyhow!("start {}: {err}", self.program.display()))?;
        log::info!("started the plugin host, pid {}", child.id());
        // Reaped when it exits, long after this returns. If this process
        // exits first, the system reaps it instead.
        let waiter = std::thread::Builder::new()
            .name("plugin-host-wait".into())
            .stack_size(64 * 1024)
            .spawn(move || {
                let _ = child.wait();
            });
        if let Err(err) = waiter {
            log::warn!("cannot wait for the plugin host: {err}");
        }
        Ok(())
    }
}

/// Nothing is bound at the socket: no file, or a file its host left behind.
fn nobody_listens(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::NotFound | ErrorKind::ConnectionRefused
    )
}

fn is_transient(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::UnexpectedEof
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe
    )
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

fn open_log(path: &Path) -> io::Result<File> {
    let long = fs::metadata(path).is_ok_and(|meta| meta.len() > LOG_LIMIT);
    OpenOptions::new()
        .create(true)
        .append(!long)
        .write(true)
        .truncate(long)
        .open(path)
}

/// One connection to the host, past its hello.
#[derive(Debug)]
pub struct Connection {
    stream: UnixStream,
}

impl Connection {
    pub fn send(&mut self, message: &ToHost) -> io::Result<()> {
        write_frame(&mut self.stream, &message.encode())
    }

    pub fn recv(&mut self) -> io::Result<FromHost> {
        let frame = read_frame(&mut self.stream)?;
        FromHost::decode(&frame).map_err(|err| io::Error::new(ErrorKind::InvalidData, err))
    }

    /// A second handle on the connection, so one thread can wait for what
    /// the host sends while another sends to it.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
        })
    }

    /// Ends the connection for every handle on it: a `recv` waiting on
    /// another thread returns.
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }

    /// The socket, for a caller that carries frames without reading them.
    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}

/// How often a keeper looks at whether any plugin runs always: soon after
/// one is chosen to, it runs.
const KEEP_LOOK: Duration = Duration::from_secs(2);
/// The pause before a keeper reaches the host again after losing it,
/// doubling while the host stays away.
const KEEP_RETRY_FIRST: Duration = Duration::from_secs(1);
const KEEP_RETRY_LAST: Duration = Duration::from_secs(30);
/// A keeper's stack: room to read a manifest nested as deep as the TOML
/// parser lets one go (80 levels), which takes 512 KiB in a release build
/// and 2 MiB in a debug one, unoptimized: twice that. A thread that runs
/// out aborts the process -- the desktop, or the mux with every session.
const KEEP_STACK: usize = if cfg!(debug_assertions) {
    4 * 1024 * 1024
} else {
    1024 * 1024
};

/// Keeps the host up while any plugin runs always, for as long as this
/// process runs, telling it this is ThinkTerm running on the machine
/// (`registry::Request::Keep`): while one is connected so, those plugins
/// run, and once none is, they stop with the host. For the desktop and the
/// mux server. Which plugins run always is the host's word, in a file of
/// its data directory ([`crate::paths::always`]): with none, no host is
/// started or kept for this -- unless a plugin whose manifest says it runs
/// always was installed where the host has not looked since, which the
/// host is then started to look at.
pub fn keep_host_up() {
    let keeping = std::thread::Builder::new()
        .name("plugin-keeper".into())
        .stack_size(KEEP_STACK)
        .spawn(|| {
            let plugins = crate::paths::plugins_dir();
            let mut manifests = Manifests::default();
            let mut retry = KEEP_RETRY_FIRST;
            loop {
                // Read first, so that the host's answer marks what it looked at.
                let unlooked = manifests.unlooked(&plugins);
                if !unlooked && !any_runs_always() {
                    std::thread::sleep(KEEP_LOOK);
                    continue;
                }
                let kept = Host::for_this_build()
                    .map_err(anyhow::Error::from)
                    .and_then(|host| host.connect())
                    .and_then(|mut connection| {
                        keep(&mut connection)?;
                        connection.stream.set_read_timeout(Some(KEEP_LOOK))?;
                        Ok(connection)
                    });
                match kept {
                    Ok(mut connection) => {
                        retry = KEEP_RETRY_FIRST;
                        // Held until the host goes, or nothing runs always
                        // any more. The host looks for new plugins at each
                        // `keep`, and has once it answers.
                        loop {
                            match connection.recv() {
                                Ok(FromHost::Ok { .. } | FromHost::Error { .. }) => {
                                    manifests.looked();
                                }
                                Ok(_) => {}
                                Err(err) if is_timeout(&err) => {
                                    if manifests.unlooked(&plugins) {
                                        if keep(&mut connection).is_err() {
                                            break;
                                        }
                                    } else if !any_runs_always() {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        connection.shutdown();
                    }
                    Err(err) => log::info!("keeping the plugin host up: {err:#}"),
                }
                std::thread::sleep(retry);
                retry = (retry * 2).min(KEEP_RETRY_LAST);
            }
        });
    if let Err(err) = keeping {
        log::warn!("cannot keep the plugin host up: {err}");
    }
}

/// Tells the host this is ThinkTerm running on the machine.
fn keep(connection: &mut Connection) -> io::Result<()> {
    connection.send(&ToHost::Call {
        id: 1,
        plugin: crate::registry::PLUGIN.into(),
        body: serde_json::json!({"op": "keep"}),
    })
}

/// Whether the host last said some plugin runs always.
fn any_runs_always() -> bool {
    std::fs::read_to_string(crate::paths::always())
        .is_ok_and(|ids| ids.lines().any(|id| !id.trim().is_empty()))
}

/// What a keeper knows of the installed plugins' manifests, each read again
/// only once it changed: whether it says its plugin runs always, and
/// whether the host has looked at it as it is since. Only the host says
/// for sure what runs always -- a plugin can be off, or not for this
/// system -- so a manifest that says so has it looked at once.
#[derive(Default)]
struct Manifests {
    known: HashMap<PathBuf, Known>,
}

struct Known {
    changed: (Option<std::time::SystemTime>, u64),
    always: bool,
    looked: bool,
}

impl Manifests {
    /// Whether a manifest under `plugins`, a plugins directory, says its
    /// plugin runs always and has not been looked at as it is.
    fn unlooked(&mut self, plugins: &Path) -> bool {
        let Ok(entries) = fs::read_dir(plugins) else {
            self.known.clear();
            return false;
        };
        let mut found = std::collections::HashSet::new();
        for entry in entries
            .filter_map(Result::ok)
            .take(crate::registry::DIR_LIMIT)
        {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let manifest = entry.path().join("plugin.toml");
            let Ok(meta) = fs::metadata(&manifest) else {
                continue;
            };
            let changed = (meta.modified().ok(), meta.len());
            if !self
                .known
                .get(&manifest)
                .is_some_and(|known| known.changed == changed)
            {
                let always = says_always(&manifest);
                self.known.insert(
                    manifest.clone(),
                    Known {
                        changed,
                        always,
                        looked: false,
                    },
                );
            }
            found.insert(manifest);
        }
        self.known.retain(|manifest, _| found.contains(manifest));
        self.known
            .values()
            .any(|known| known.always && !known.looked)
    }

    /// The host has looked at every manifest as it last was.
    fn looked(&mut self) {
        for known in self.known.values_mut() {
            known.looked = true;
        }
    }
}

/// Whether the manifest at `path` says its plugin runs always.
fn says_always(path: &Path) -> bool {
    #[derive(serde::Deserialize)]
    struct Manifest {
        #[serde(default)]
        run: Option<Run>,
    }
    #[derive(serde::Deserialize)]
    struct Run {
        #[serde(default)]
        background: Option<String>,
    }
    fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<Manifest>(&text).ok())
        .and_then(|manifest| manifest.run?.background)
        .is_some_and(|background| background == "always")
}

fn is_timeout(err: &io::Error) -> bool {
    matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// A call's answer, or why there is none.
pub type Answer = Result<Value, String>;

type Reply = Box<dyn FnOnce(Answer) + Send>;

/// The pause before trying a host that could not be reached or went away,
/// doubling up to the second while it stays away.
const SESSION_RETRY_FIRST: Duration = Duration::from_millis(250);
const SESSION_RETRY_LAST: Duration = Duration::from_secs(30);
/// How long a call waits for its answer before it is told there is none:
/// a host that hangs must not leave a save waiting for good.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// A lasting connection to the host, for a client that stays on a plugin --
/// a panel on show -- rather than asking it once. It runs on a thread of its
/// own: the host is started when it is not running, a lost connection is
/// made again with a pause that grows while the host keeps going away, and
/// every call is answered, with an error when the host cannot answer it.
/// A plugin's panel lives on one connection: one lost takes its panels
/// with it, and the owner opens them again when told it is connected.
/// Dropping the session ends it.
pub struct Session {
    jobs: Sender<Job>,
    call_timeout: Duration,
}

/// A call on its way: to which plugin, what, how long it may wait for its
/// answer, and whom to tell.
struct Call {
    plugin: String,
    body: Value,
    wait: Duration,
    reply: Reply,
}

enum Job {
    Call(Call),
    Panel(u64, PanelRequest),
    /// What the host sent over connection number `.0`.
    Heard(u64, io::Result<FromHost>),
    Stop,
}

/// What a session tells its owner, on the session's thread.
#[derive(Debug)]
pub enum Notice {
    /// Connected, for the first time or `again`: whatever the owner follows
    /// is to be asked for anew, and, connected again, its panels opened
    /// anew -- the last connection took them with it.
    Connected { again: bool },
    /// The host cannot be reached, and why. The session keeps trying.
    Trouble(String),
    /// Something a plugin told the clients watching it.
    Event { plugin: String, body: Value },
    /// About the owner's panel `view`.
    Panel { view: u64, event: PanelEvent },
}

impl Session {
    pub fn start(host: Host, notice: impl FnMut(Notice) + Send + 'static) -> io::Result<Self> {
        Self::start_with(host, CALL_TIMEOUT, notice)
    }

    /// A session whose calls wait `call_timeout` for their answers.
    pub fn start_with(
        host: Host,
        call_timeout: Duration,
        notice: impl FnMut(Notice) + Send + 'static,
    ) -> io::Result<Self> {
        let (jobs, queue) = mpsc::channel();
        let readers = jobs.clone();
        std::thread::Builder::new()
            .name("plugin-session".into())
            .spawn(move || serve_session(host, queue, readers, notice))?;
        Ok(Self { jobs, call_timeout })
    }

    /// Asks `plugin`. `reply` is called with the answer on the session's
    /// thread, or with why there is none.
    pub fn call(&self, plugin: &str, body: Value, reply: impl FnOnce(Answer) + Send + 'static) {
        self.call_within(plugin, body, self.call_timeout, reply)
    }

    /// Asks `plugin`, waiting `wait` for the answer: for a call that is
    /// known to take long.
    pub fn call_within(
        &self,
        plugin: &str,
        body: Value,
        wait: Duration,
        reply: impl FnOnce(Answer) + Send + 'static,
    ) {
        let job = Job::Call(Call {
            plugin: plugin.to_string(),
            body,
            wait,
            reply: Box::new(reply),
        });
        if let Err(mpsc::SendError(Job::Call(call))) = self.jobs.send(job) {
            (call.reply)(Err("the plugin session has ended".into()));
        }
    }

    /// Tells the host about the owner's panel `view`. Made while there is
    /// no connection, it goes nowhere: the panels are opened again once
    /// there is.
    pub fn panel(&self, view: u64, request: PanelRequest) {
        let _ = self.jobs.send(Job::Panel(view, request));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.jobs.send(Job::Stop);
    }
}

/// A call sent and not yet answered: when it gives up, how long that was,
/// and whom to tell.
struct Waiter {
    deadline: Instant,
    wait: Duration,
    reply: Reply,
}

type Waiting = HashMap<u64, Waiter>;

fn serve_session(
    host: Host,
    queue: Receiver<Job>,
    readers: Sender<Job>,
    mut notice: impl FnMut(Notice),
) {
    let mut next_id = 0u64;
    let mut generation = 0u64;
    let mut retry = SESSION_RETRY_FIRST;
    // Calls made while there was no connection, sent on the next one.
    let mut queued: Vec<Call> = Vec::new();
    // Something about a panel went nowhere: the owner opens its panels
    // again on the next connection, the first one included.
    let mut panels_lost = false;
    loop {
        let connected = host.connect().and_then(|connection| {
            let reader = connection.try_clone()?;
            Ok((connection, reader))
        });
        let (mut connection, mut reader) = match connected {
            Ok(handles) => handles,
            Err(err) => {
                let why = format!("{err:#}");
                for call in queued.drain(..) {
                    (call.reply)(Err(why.clone()));
                }
                notice(Notice::Trouble(why));
                if !pause(&queue, retry, &mut queued, &mut panels_lost) {
                    return;
                }
                retry = (retry * 2).min(SESSION_RETRY_LAST);
                continue;
            }
        };
        generation += 1;
        let heard = readers.clone();
        let this = generation;
        let listening = std::thread::Builder::new()
            .name("plugin-session-read".into())
            .spawn(move || loop {
                let message = reader.recv();
                let ended = message.is_err();
                if heard.send(Job::Heard(this, message)).is_err() || ended {
                    return;
                }
            });
        if let Err(err) = listening {
            log::warn!("cannot read from the plugin host: {err}");
            connection.shutdown();
            if !pause(&queue, retry, &mut queued, &mut panels_lost) {
                return;
            }
            continue;
        }
        notice(Notice::Connected {
            again: generation > 1 || panels_lost,
        });
        panels_lost = false;

        let mut waiting = Waiting::new();
        let mut alive = true;
        for call in queued.drain(..) {
            if alive {
                alive = send_call(&mut connection, &mut next_id, &mut waiting, call);
            } else {
                (call.reply)(Err("the plugin host went away".into()));
            }
        }
        while alive {
            let job = match waiting.values().map(|waiter| waiter.deadline).min() {
                Some(deadline) => {
                    match queue.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(job) => Ok(job),
                        Err(RecvTimeoutError::Timeout) => {
                            give_up_overdue(&mut waiting);
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => Err(()),
                    }
                }
                None => queue.recv().map_err(drop),
            };
            match job {
                Ok(Job::Call(call)) => {
                    alive = send_call(&mut connection, &mut next_id, &mut waiting, call)
                }
                Ok(Job::Panel(view, request)) => {
                    alive = connection.send(&ToHost::Panel { view, request }).is_ok();
                    panels_lost |= !alive;
                }
                Ok(Job::Heard(from, _)) if from != generation => {}
                Ok(Job::Heard(_, Ok(FromHost::Ok { id, body }))) => {
                    retry = SESSION_RETRY_FIRST;
                    if let Some(waiter) = waiting.remove(&id) {
                        (waiter.reply)(Ok(body));
                    }
                }
                Ok(Job::Heard(_, Ok(FromHost::Error { id, message }))) => {
                    if let Some(waiter) = waiting.remove(&id) {
                        (waiter.reply)(Err(message));
                    }
                }
                Ok(Job::Heard(_, Ok(FromHost::Event { plugin, body }))) => {
                    notice(Notice::Event { plugin, body })
                }
                Ok(Job::Heard(_, Ok(FromHost::Panel { view, event }))) => {
                    notice(Notice::Panel { view, event })
                }
                Ok(Job::Heard(_, Ok(FromHost::Hello { .. }))) => {}
                Ok(Job::Heard(_, Err(_))) => alive = false,
                Ok(Job::Stop) | Err(()) => {
                    connection.shutdown();
                    for (_, waiter) in waiting.drain() {
                        (waiter.reply)(Err("the plugin session has ended".into()));
                    }
                    return;
                }
            }
        }
        connection.shutdown();
        for (_, waiter) in waiting.drain() {
            (waiter.reply)(Err("the plugin host went away".into()));
        }
        // A host that answered reset this; one that keeps failing is asked
        // less and less often.
        if !pause(&queue, retry, &mut queued, &mut panels_lost) {
            return;
        }
        retry = (retry * 2).min(SESSION_RETRY_LAST);
    }
}

/// Sends a call, to be answered through its reply once its wait is up at
/// the latest. False when the host has gone, which the reply is told.
fn send_call(
    connection: &mut Connection,
    next_id: &mut u64,
    waiting: &mut Waiting,
    call: Call,
) -> bool {
    *next_id += 1;
    let message = ToHost::Call {
        id: *next_id,
        plugin: call.plugin,
        body: call.body,
    };
    match connection.send(&message) {
        Ok(()) => {
            let waiter = Waiter {
                deadline: Instant::now() + call.wait,
                wait: call.wait,
                reply: call.reply,
            };
            waiting.insert(*next_id, waiter);
            true
        }
        Err(err) => {
            (call.reply)(Err(format!("the plugin host went away: {err}")));
            false
        }
    }
}

/// Tells the calls whose time is up that no answer is coming. One that
/// arrives later finds nobody waiting and is dropped.
fn give_up_overdue(waiting: &mut Waiting) {
    let now = Instant::now();
    let overdue: Vec<u64> = waiting
        .iter()
        .filter(|(_, waiter)| waiter.deadline <= now)
        .map(|(id, _)| *id)
        .collect();
    for id in overdue {
        if let Some(waiter) = waiting.remove(&id) {
            (waiter.reply)(Err(format!(
                "the plugin host did not answer within {:?}",
                waiter.wait
            )));
        }
    }
}

/// Waits `delay` before the next attempt, keeping the calls made meanwhile;
/// a call cuts the wait short, since someone is waiting on it. What is said
/// about a panel meanwhile goes nowhere, and `panels_lost` says so. False
/// when the session was ended.
fn pause(
    queue: &Receiver<Job>,
    delay: Duration,
    queued: &mut Vec<Call>,
    panels_lost: &mut bool,
) -> bool {
    let until = Instant::now() + delay;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        match queue.recv_timeout(left) {
            Ok(Job::Call(call)) => {
                queued.push(call);
                return true;
            }
            Ok(Job::Heard(..)) => {}
            Ok(Job::Panel(..)) => *panels_lost = true,
            Ok(Job::Stop) | Err(RecvTimeoutError::Disconnected) => return false,
            Err(RecvTimeoutError::Timeout) => return true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_that_says_it_runs_always_is_looked_at_once_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let plugins = dir.path().join("plugins");
        let mut manifests = Manifests::default();
        assert!(!manifests.unlooked(&plugins), "nothing installed");
        let install = |name: &str, run: &str| {
            let at = plugins.join(name);
            fs::create_dir_all(&at).unwrap();
            let manifest = format!("id = \"{name}\"\nname = \"N\"\napi = 1\n[run]\n{run}\n");
            fs::write(at.join("plugin.toml"), manifest).unwrap();
        };
        install("a", "program = \"a\"");
        install("b", "program = \"b\"\nbackground = \"never\"");
        fs::create_dir_all(plugins.join(".hidden")).unwrap();
        assert!(!manifests.unlooked(&plugins), "none says it runs always");

        install("c", "program = \"c\"\nbackground = \"always\"");
        assert!(manifests.unlooked(&plugins), "the host has to say");
        assert!(manifests.unlooked(&plugins), "until it has looked");
        manifests.looked();
        assert!(!manifests.unlooked(&plugins), "looked at, as it is");

        install("c", "program = \"c2\"\nbackground = \"always\"");
        assert!(manifests.unlooked(&plugins), "changed since");
        manifests.looked();
        fs::remove_dir_all(plugins.join("c")).unwrap();
        assert!(!manifests.unlooked(&plugins));
        assert_eq!(manifests.known.len(), 2, "what is gone is let go of");
    }

    #[test]
    fn a_manifest_nested_as_deep_as_toml_goes_is_read_on_a_keepers_stack() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin.toml");
        // The parser takes 80 levels, and refuses more.
        let deepest = |open: &str, close: &str| {
            format!(
                "id = \"a\"\nx = {}1{}\n[run]\nbackground = \"always\"\n",
                open.repeat(79),
                close.repeat(79)
            )
        };
        for manifest in [deepest("[", "]"), deepest("{a=", "}")] {
            fs::write(&path, manifest).unwrap();
            let path = path.clone();
            let read = std::thread::Builder::new()
                .stack_size(KEEP_STACK)
                .spawn(move || says_always(&path))
                .unwrap()
                .join()
                .unwrap();
            assert!(read, "read, not aborted");
        }
    }
}
