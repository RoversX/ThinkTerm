//! Reaching the host: connecting to it, starting it when nothing answers,
//! and replacing one that speaks an older protocol.

use crate::wire::{read_frame, write_frame, FromHost, ToHost, PROTOCOL};
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
/// Dropping the session ends it.
pub struct Session {
    jobs: Sender<Job>,
}

enum Job {
    Call {
        plugin: String,
        body: Value,
        reply: Reply,
    },
    /// What the host sent over connection number `.0`.
    Heard(u64, io::Result<FromHost>),
    Stop,
}

/// What a session tells its owner, on the session's thread.
#[derive(Debug)]
pub enum Notice {
    /// Connected, for the first time or again: whatever the owner follows
    /// is to be asked for anew.
    Connected,
    /// The host cannot be reached, and why. The session keeps trying.
    Trouble(String),
    /// Something a plugin told the clients watching it.
    Event { plugin: String, body: Value },
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
            .spawn(move || serve_session(host, call_timeout, queue, readers, notice))?;
        Ok(Self { jobs })
    }

    /// Asks `plugin`. `reply` is called with the answer on the session's
    /// thread, or with why there is none.
    pub fn call(&self, plugin: &str, body: Value, reply: impl FnOnce(Answer) + Send + 'static) {
        let job = Job::Call {
            plugin: plugin.to_string(),
            body,
            reply: Box::new(reply),
        };
        if let Err(mpsc::SendError(Job::Call { reply, .. })) = self.jobs.send(job) {
            reply(Err("the plugin session has ended".into()));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.jobs.send(Job::Stop);
    }
}

/// The calls sent and not yet answered: when each gives up, and whom to
/// tell.
type Waiting = HashMap<u64, (Instant, Reply)>;

fn serve_session(
    host: Host,
    call_timeout: Duration,
    queue: Receiver<Job>,
    readers: Sender<Job>,
    mut notice: impl FnMut(Notice),
) {
    let mut next_id = 0u64;
    let mut generation = 0u64;
    let mut retry = SESSION_RETRY_FIRST;
    // Calls made while there was no connection, sent on the next one.
    let mut queued: Vec<(String, Value, Reply)> = Vec::new();
    loop {
        let connected = host.connect().and_then(|connection| {
            let reader = connection.try_clone()?;
            Ok((connection, reader))
        });
        let (mut connection, mut reader) = match connected {
            Ok(handles) => handles,
            Err(err) => {
                let why = format!("{err:#}");
                for (_, _, reply) in queued.drain(..) {
                    reply(Err(why.clone()));
                }
                notice(Notice::Trouble(why));
                if !pause(&queue, retry, &mut queued) {
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
            if !pause(&queue, retry, &mut queued) {
                return;
            }
            continue;
        }
        notice(Notice::Connected);

        let mut waiting = Waiting::new();
        let mut alive = true;
        for (plugin, body, reply) in queued.drain(..) {
            if alive {
                let deadline = Instant::now() + call_timeout;
                alive = send_call(
                    &mut connection,
                    &mut next_id,
                    &mut waiting,
                    (plugin, body, reply),
                    deadline,
                );
            } else {
                reply(Err("the plugin host went away".into()));
            }
        }
        while alive {
            let job = match waiting.values().map(|(deadline, _)| *deadline).min() {
                Some(deadline) => {
                    match queue.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                        Ok(job) => Ok(job),
                        Err(RecvTimeoutError::Timeout) => {
                            give_up_overdue(&mut waiting, call_timeout);
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => Err(()),
                    }
                }
                None => queue.recv().map_err(drop),
            };
            match job {
                Ok(Job::Call {
                    plugin,
                    body,
                    reply,
                }) => {
                    let deadline = Instant::now() + call_timeout;
                    alive = send_call(
                        &mut connection,
                        &mut next_id,
                        &mut waiting,
                        (plugin, body, reply),
                        deadline,
                    )
                }
                Ok(Job::Heard(from, _)) if from != generation => {}
                Ok(Job::Heard(_, Ok(FromHost::Ok { id, body }))) => {
                    retry = SESSION_RETRY_FIRST;
                    if let Some((_, reply)) = waiting.remove(&id) {
                        reply(Ok(body));
                    }
                }
                Ok(Job::Heard(_, Ok(FromHost::Error { id, message }))) => {
                    if let Some((_, reply)) = waiting.remove(&id) {
                        reply(Err(message));
                    }
                }
                Ok(Job::Heard(_, Ok(FromHost::Event { plugin, body }))) => {
                    notice(Notice::Event { plugin, body })
                }
                Ok(Job::Heard(_, Ok(FromHost::Hello { .. }))) => {}
                Ok(Job::Heard(_, Err(_))) => alive = false,
                Ok(Job::Stop) | Err(()) => {
                    connection.shutdown();
                    for (_, (_, reply)) in waiting.drain() {
                        reply(Err("the plugin session has ended".into()));
                    }
                    return;
                }
            }
        }
        connection.shutdown();
        for (_, (_, reply)) in waiting.drain() {
            reply(Err("the plugin host went away".into()));
        }
        // A host that answered reset this; one that keeps failing is asked
        // less and less often.
        if !pause(&queue, retry, &mut queued) {
            return;
        }
        retry = (retry * 2).min(SESSION_RETRY_LAST);
    }
}

/// Sends a call, to be answered through `reply` by `deadline`. False when
/// the host has gone, which `reply` is told.
fn send_call(
    connection: &mut Connection,
    next_id: &mut u64,
    waiting: &mut Waiting,
    (plugin, body, reply): (String, Value, Reply),
    deadline: Instant,
) -> bool {
    *next_id += 1;
    let call = ToHost::Call {
        id: *next_id,
        plugin,
        body,
    };
    match connection.send(&call) {
        Ok(()) => {
            waiting.insert(*next_id, (deadline, reply));
            true
        }
        Err(err) => {
            reply(Err(format!("the plugin host went away: {err}")));
            false
        }
    }
}

/// Tells the calls whose time is up that no answer is coming. One that
/// arrives later finds nobody waiting and is dropped.
fn give_up_overdue(waiting: &mut Waiting, call_timeout: Duration) {
    let now = Instant::now();
    let overdue: Vec<u64> = waiting
        .iter()
        .filter(|(_, (deadline, _))| *deadline <= now)
        .map(|(id, _)| *id)
        .collect();
    for id in overdue {
        if let Some((_, reply)) = waiting.remove(&id) {
            reply(Err(format!(
                "the plugin host did not answer within {call_timeout:?}"
            )));
        }
    }
}

/// Waits `delay` before the next attempt, keeping the calls made meanwhile;
/// a call cuts the wait short, since someone is waiting on it. False when
/// the session was ended.
fn pause(queue: &Receiver<Job>, delay: Duration, queued: &mut Vec<(String, Value, Reply)>) -> bool {
    let until = Instant::now() + delay;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        match queue.recv_timeout(left) {
            Ok(Job::Call {
                plugin,
                body,
                reply,
            }) => {
                queued.push((plugin, body, reply));
                return true;
            }
            Ok(Job::Heard(..)) => {}
            Ok(Job::Stop) | Err(RecvTimeoutError::Disconnected) => return false,
            Err(RecvTimeoutError::Timeout) => return true,
        }
    }
}
