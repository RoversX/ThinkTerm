//! The machine's ThinkTerm processes reach the tailnet through the one
//! holding the node, `thinkterm-tailnet`: the app, `thinkterm ssh` and the
//! mux server. A connection is asked for on
//! a socket beside the settings that only this user can open, with a line
//! naming the host; the answer is a line too, and after "ok" the socket
//! carries the connection itself. Asking needs nothing of the library: only
//! the holder, built with the `node` feature, answers.
//!
//! On Windows the socket is AF_UNIX too, as the mux server's is there
//! (Windows 10 1803 and later have it; ThinkTerm needs 1809). A connection
//! through it is then a socket the SSH libraries take as they take a TCP
//! one, where a named pipe would have to be copied through a socket of its
//! own; and it is a file in this user's own folder, where a pipe's name is
//! in one namespace for the whole machine, for another user to take first.

use crate::status::Family;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Read, Write};
#[cfg(all(feature = "node", unix))]
use std::os::unix::net::UnixListener;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};
#[cfg(all(feature = "node", windows))]
use wezterm_uds::UnixListener;
#[cfg(windows)]
use wezterm_uds::UnixStream;
#[cfg(feature = "node")]
use {
    crate::node::{self, Shared},
    std::net::SocketAddr,
    std::path::PathBuf,
    std::sync::Arc,
    tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
};

/// A request or an answer longer than this is not one.
const LINE_MAX: usize = 4096;

/// How long reaching the holder's socket may take: at once while it takes
/// connections, never while a stuck holder lets its backlog fill.
const CONNECT_WAIT: Duration = Duration::from_secs(2);

/// How long past the time it was given the holder may take to answer: its
/// last look the name up can start just before that runs out.
const ANSWER_SLACK: Duration = Duration::from_secs(5);

/// How often an asker waiting for the answer looks whether its session was
/// closed meanwhile.
const LOOK: Duration = Duration::from_millis(100);

/// The local leg of each connection gets this much buffer each way: macOS
/// gives a socket pair 8 KiB, which holds a copy over SSH far below what
/// the tailnet carries.
const SOCKET_BUFFER: usize = 512 * 1024;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Request {
    host: String,
    port: u16,
    /// Missing from the requests of a process older than the holder.
    #[serde(default)]
    family: Family,
    timeout_ms: u64,
}

/// Gives a local socket the room SOCKET_BUFFER says. Best effort: one
/// that keeps the system's default still works, only slower.
#[cfg(unix)]
pub(crate) fn widen(socket: &impl std::os::fd::AsFd) {
    let socket = socket2::SockRef::from(socket);
    let _ = socket.set_send_buffer_size(SOCKET_BUFFER);
    let _ = socket.set_recv_buffer_size(SOCKET_BUFFER);
}

#[cfg(windows)]
pub(crate) fn widen(socket: &impl std::os::windows::io::AsSocket) {
    let socket = socket2::SockRef::from(socket);
    let _ = socket.set_send_buffer_size(SOCKET_BUFFER);
    let _ = socket.set_recv_buffer_size(SOCKET_BUFFER);
}

/// What asking the holder of the node came to.
pub(crate) enum Asked {
    /// Nobody listens at `path`: no process here holds a node.
    Nobody,
    /// The holder went without answering, as one does whose node is on its
    /// way out (see `answer`): the node started in its place may answer.
    Gone,
    /// A connection, why there is none, or None when `host` is none of its
    /// machines or the holder kept its answer past any wait it could have.
    Answered(Option<Result<socket2::Socket>>),
}

/// What reading the holder's answer came to.
enum Answer {
    Line(String),
    /// It went without one.
    Gone,
    /// It kept it past any wait it could have.
    Late,
}

/// A connection to `host:port` through the node of the process listening
/// at `path`, which has `timeout` to make it.
pub(crate) fn ask(
    path: &Path,
    host: &str,
    port: u16,
    family: Family,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Asked {
    let Some(mut stream) = connect_within(path, CONNECT_WAIT) else {
        return Asked::Nobody;
    };
    let request = Request {
        host: host.to_string(),
        port,
        family,
        timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
    };
    let Ok(mut line) = serde_json::to_vec(&request) else {
        return Asked::Answered(None);
    };
    line.push(b'\n');
    // Closed before it read the request: it went without answering.
    if stream.write_all(&line).is_err() {
        return Asked::Gone;
    }
    let answer = match read_answer(&mut stream, timeout, cancelled) {
        Ok(Answer::Line(answer)) => answer,
        Ok(Answer::Gone) => return Asked::Gone,
        // The selected network cannot connect.
        Ok(Answer::Late) => return Asked::Answered(None),
        Err(err) => return Asked::Answered(Some(Err(err))),
    };
    Asked::Answered(match answer.as_str() {
        "none" => None,
        "ok" => {
            widen(&stream);
            Some(into_socket(stream).context("taking over the tailnet's connection"))
        }
        answer => Some(Err(anyhow!(
            "{}",
            answer.strip_prefix("error: ").unwrap_or(answer)
        ))),
    })
}

/// A stream to the socket at `path`, if something listens there and takes
/// it within `wait`.
#[cfg(unix)]
fn connect_within(path: &Path, wait: Duration) -> Option<UnixStream> {
    let address = socket2::SockAddr::unix(path).ok()?;
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None).ok()?;
    socket.connect_timeout(&address, wait).ok()?;
    socket.set_nonblocking(false).ok()?;
    Some(UnixStream::from(std::os::fd::OwnedFd::from(socket)))
}

/// Windows refuses a connection that a listener has no room for at once,
/// rather than keeping it waiting for room, so there is no wait to bound.
#[cfg(windows)]
fn connect_within(path: &Path, wait: Duration) -> Option<UnixStream> {
    let _ = wait;
    UnixStream::connect_without_waiting(path).ok()
}

/// The connection the holder said "ok" to, as the SSH dialer takes it.
#[cfg(unix)]
fn into_socket(stream: UnixStream) -> std::io::Result<socket2::Socket> {
    stream.set_read_timeout(None)?;
    Ok(socket2::Socket::from(std::os::fd::OwnedFd::from(stream)))
}

/// On Windows the answer was waited for without a timeout on the socket
/// (see `read_byte`), so there is none to take off.
#[cfg(windows)]
fn into_socket(stream: UnixStream) -> std::io::Result<socket2::Socket> {
    use std::os::windows::io::{FromRawSocket, IntoRawSocket, OwnedSocket};
    // The stream gives its socket up, to be the one owner of it.
    let socket = unsafe { OwnedSocket::from_raw_socket(stream.into_raw_socket()) };
    Ok(socket2::Socket::from(socket))
}

/// The holder's answer, read a byte at a time: what follows it is the
/// connection's own.
fn read_answer(
    stream: &mut UnixStream,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<Answer> {
    if !look_while_reading(stream) {
        return Ok(Answer::Gone);
    }
    // The holder waits for its node to get in and for the other machine
    // within `timeout`.
    let deadline = Instant::now().checked_add(timeout.saturating_add(ANSWER_SLACK));
    let mut line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        match read_byte(stream, &mut byte) {
            Ok(0) => return Ok(Answer::Gone),
            Ok(_) if byte[0] == b'\n' => {
                return Ok(Answer::Line(String::from_utf8_lossy(&line).into_owned()))
            }
            Ok(_) if line.len() < LINE_MAX => line.push(byte[0]),
            Ok(_) => bail!("the tailnet's node gave no answer it could read"),
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                if cancelled() {
                    bail!("the session was closed while connecting");
                }
                // The caller turns an unanswered request into a failure.
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    log::warn!("tailnet: the process holding the node did not answer");
                    return Ok(Answer::Late);
                }
            }
            Err(_) => return Ok(Answer::Gone),
        }
    }
}

/// Has each read of the answer give up after LOOK, for the asker to look
/// whether its session was closed. False when the connection has already
/// gone: macOS sets no timeout on a connection whose other end has closed.
#[cfg(unix)]
fn look_while_reading(stream: &UnixStream) -> bool {
    stream.set_read_timeout(Some(LOOK)).is_ok()
}

/// On Windows the wait is the one in `read_byte`.
#[cfg(windows)]
fn look_while_reading(stream: &UnixStream) -> bool {
    let _ = stream;
    true
}

/// A byte of the answer, or TimedOut once LOOK has passed without one.
#[cfg(unix)]
fn read_byte(stream: &mut UnixStream, byte: &mut [u8; 1]) -> std::io::Result<usize> {
    stream.read(byte)
}

/// Winsock has a receive that runs out of time leave its connection "in an
/// indeterminate state", and this one carries on after it; so the wait is
/// for something to read, and the read only comes after that.
#[cfg(windows)]
fn read_byte(stream: &mut UnixStream, byte: &mut [u8; 1]) -> std::io::Result<usize> {
    use std::os::windows::io::AsRawSocket;
    let mut wanted = [filedescriptor::pollfd {
        fd: stream.as_raw_socket() as _,
        events: filedescriptor::POLLIN,
        revents: 0,
    }];
    match filedescriptor::poll(&mut wanted, Some(LOOK)) {
        Ok(0) => Err(ErrorKind::TimedOut.into()),
        // Something to read, the end of the connection or an error: the
        // read says which.
        Ok(_) => stream.read(byte),
        Err(err) => Err(std::io::Error::other(err)),
    }
}

/// The socket for a node starting in this process. This process holds the
/// node (see settings::try_hold_node), so a socket already there is one a
/// holder that has gone left behind.
///
/// Windows has no mode to give the file: it takes the folder's
/// permissions, which under the user's profile let in this user, SYSTEM
/// and the Administrators group alone (see crate::settings), and `serve`
/// turns away any process that is not this user's.
#[cfg(feature = "node")]
pub(crate) fn bind() -> Result<(UnixListener, PathBuf)> {
    let path = crate::settings::socket_path()?;
    if let Some(parent) = path.parent() {
        crate::settings::create_private_dir(parent)?;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("removing {}", path.display())),
    }
    let listener =
        UnixListener::bind(&path).with_context(|| format!("listening on {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("making {} private", path.display()))?;
        listener.set_nonblocking(true)?;
    }
    Ok((listener, path))
}

/// Answers the machine's other processes for as long as the node's runtime
/// runs.
#[cfg(all(feature = "node", unix))]
pub(crate) async fn serve(shared: Arc<Shared>, listener: UnixListener) {
    let listener = match tokio::net::UnixListener::from_std(listener) {
        Ok(listener) => listener,
        Err(err) => {
            log::warn!("tailnet: sharing the node with this machine's other processes: {err}");
            return;
        }
    };
    let user = nix::unistd::Uid::current().as_raw();
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                // This user's processes only, as the socket's mode says too.
                if stream.peer_cred().is_ok_and(|peer| peer.uid() == user) {
                    tokio::spawn(answer(Arc::clone(&shared), stream));
                } else {
                    tokio::spawn(refuse(stream));
                }
            }
            Err(err) => {
                log::warn!("tailnet: taking a connection from another process: {err}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Answers the machine's other processes for as long as the node's runtime
/// runs. Tokio has no AF_UNIX on Windows: the listener and the connections
/// it takes are async-io's, whose own thread watches them, and the node's
/// side of each is the Tokio one through tokio-util's compat. All of it
/// goes with the runtime, as on unix.
#[cfg(all(feature = "node", windows))]
pub(crate) async fn serve(shared: Arc<Shared>, listener: UnixListener) {
    use tokio_util::compat::FuturesAsyncReadCompatExt;
    let listener = match async_io::Async::new(listener) {
        Ok(listener) => listener,
        Err(err) => {
            log::warn!("tailnet: sharing the node with this machine's other processes: {err}");
            return;
        }
    };
    loop {
        match listener.read_with(|listener| listener.accept()).await {
            Ok((stream, _)) => {
                // This user's processes only, as the folder the socket is
                // in lets in too.
                let theirs = crate::windows::is_this_users(&stream);
                match async_io::Async::new(stream) {
                    Ok(stream) if theirs => {
                        tokio::spawn(answer(Arc::clone(&shared), stream.compat()));
                    }
                    Ok(stream) => {
                        tokio::spawn(refuse(stream.compat()));
                    }
                    Err(err) => {
                        log::warn!("tailnet: taking a connection from another process: {err}")
                    }
                }
            }
            Err(err) => {
                log::warn!("tailnet: taking a connection from another process: {err}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// A connection from another process, as the node answers it.
#[cfg(feature = "node")]
trait Local: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    /// `widen`, for the socket underneath.
    fn make_room(&self);
}

#[cfg(all(feature = "node", unix))]
impl Local for tokio::net::UnixStream {
    fn make_room(&self) {
        widen(self);
    }
}

#[cfg(all(feature = "node", windows))]
impl Local for tokio_util::compat::Compat<async_io::Async<wezterm_uds::UnixStream>> {
    fn make_room(&self) {
        widen(self.get_ref().get_ref());
    }
}

#[cfg(feature = "node")]
async fn answer(shared: Arc<Shared>, mut stream: impl Local) {
    let Ok(Ok(request)) =
        tokio::time::timeout(Duration::from_secs(5), read_request(&mut stream)).await
    else {
        return;
    };
    let timeout = Duration::from_millis(request.timeout_ms);
    let started = std::time::Instant::now();
    let resolved = {
        let shared = Arc::clone(&shared);
        let host = request.host.clone();
        let family = request.family;
        tokio::task::spawn_blocking(move || {
            shared.resolve(&host, family, node::starting_wait(timeout), &|| false)
        })
        .await
    };
    // A node on its way out answers nothing, rather than that it knows no
    // such machine or cannot reach it: the asker asks the node in its place.
    let ip = match resolved {
        Ok(Some(ip)) => ip,
        Ok(None) if shared.is_going() => return,
        Ok(None) => {
            let _ = stream.write_all(b"none\n").await;
            return;
        }
        Err(_) => return,
    };
    // What is left of the time, for the connection itself.
    let left = timeout.saturating_sub(started.elapsed());
    match shared
        .tcp_connect(SocketAddr::new(ip, request.port), left)
        .await
    {
        Ok(tailnet) => {
            if stream.write_all(b"ok\n").await.is_ok() {
                stream.make_room();
                node::bridge(tailnet, stream).await;
            }
        }
        Err(_) if shared.is_going() => {}
        Err(err) => {
            let err = err.context(format!(
                "connecting to {}:{} through the tailnet",
                request.host, request.port
            ));
            let line = format!("error: {}\n", format!("{err:#}").replace('\n', " "));
            let _ = stream.write_all(line.as_bytes()).await;
        }
    }
}

/// Turns away a process that is not this user's, saying so: it fails at
/// once with the reason, rather than take the silence for a holder on its
/// way out and ask again until it gives up. Its request is read first: a
/// connection closed with something in it unread may be reset, and the
/// answer lost with it.
#[cfg(feature = "node")]
async fn refuse(mut stream: impl Local) {
    let _ = tokio::time::timeout(Duration::from_secs(5), read_request(&mut stream)).await;
    let _ = stream
        .write_all(
            b"error: turned away by this machine's tailnet node, which answers \
              its own user's programs only (its log says why)\n",
        )
        .await;
}

#[cfg(feature = "node")]
async fn read_request(stream: &mut impl Local) -> Result<Request> {
    let mut line = Vec::new();
    loop {
        match stream.read_u8().await? {
            b'\n' => break,
            _ if line.len() >= LINE_MAX => bail!("not a request"),
            byte => line.push(byte),
        }
    }
    Ok(serde_json::from_slice(&line)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    #[cfg(windows)]
    use wezterm_uds::UnixListener;

    /// A folder of its own for a socket. Socket paths are short; the
    /// default temp dir on macOS is not.
    #[cfg(unix)]
    fn socket_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("tt-")
            .tempdir_in("/tmp")
            .unwrap()
    }

    #[cfg(windows)]
    fn socket_dir() -> tempfile::TempDir {
        tempfile::Builder::new().prefix("tt-").tempdir().unwrap()
    }

    /// The connection `ask` handed over, as a stream again.
    #[cfg(unix)]
    fn stream_of(socket: socket2::Socket) -> UnixStream {
        UnixStream::from(std::os::fd::OwnedFd::from(socket))
    }

    #[cfg(windows)]
    fn stream_of(socket: socket2::Socket) -> UnixStream {
        use std::os::windows::io::{FromRawSocket, IntoRawSocket};
        // The socket's one owner hands it on.
        unsafe { UnixStream::from_raw_socket(socket.into_raw_socket()) }
    }

    /// A holder that answers the one request it gets with `answer`, then
    /// echoes whatever comes after. Returns the socket's path and the
    /// request it got.
    fn holder(
        answer: &'static str,
    ) -> (tempfile::TempDir, PathBuf, std::thread::JoinHandle<Request>) {
        let dir = socket_dir();
        let path = dir.path().join("tailnet.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let served = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while stream.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
                line.push(byte[0]);
            }
            stream.write_all(answer.as_bytes()).unwrap();
            let mut rest = [0u8; 4];
            if stream.read_exact(&mut rest).is_ok() {
                stream.write_all(&rest).unwrap();
            }
            serde_json::from_slice(&line).unwrap()
        });
        (dir, path, served)
    }

    /// What the holder at `path` answers for `host`, port 22.
    fn answer(path: &Path, host: &str, family: Family) -> Option<Result<socket2::Socket>> {
        match ask(path, host, 22, family, Duration::from_secs(5), &|| false) {
            Asked::Answered(answered) => answered,
            Asked::Nobody => panic!("nobody listened at {}", path.display()),
            Asked::Gone => panic!("the holder at {} went without answering", path.display()),
        }
    }

    #[test]
    fn a_connection_through_another_process_carries_on_after_its_ok() {
        let (_dir, path, served) = holder("ok\n");
        let socket = answer(&path, "server-a", Family::V6)
            .expect("the holder knew it")
            .unwrap();
        let mut stream = stream_of(socket);
        stream.write_all(b"ping").unwrap();
        let mut echoed = [0u8; 4];
        stream.read_exact(&mut echoed).unwrap();
        assert_eq!(&echoed, b"ping");
        assert_eq!(
            served.join().unwrap(),
            Request {
                host: "server-a".into(),
                port: 22,
                family: Family::V6,
                timeout_ms: 5000
            }
        );
    }

    #[test]
    fn a_request_without_a_family_allows_any() {
        let request: Request =
            serde_json::from_str(r#"{"host":"server-a","port":22,"timeout_ms":5000}"#).unwrap();
        assert_eq!(request.family, Family::Any);
    }

    #[test]
    fn an_unknown_host_gets_no_connection_from_the_holder() {
        let (_dir, path, served) = holder("none\n");
        assert!(answer(&path, "example.com", Family::Any).is_none());
        served.join().unwrap();
    }

    #[test]
    fn the_other_process_says_why_it_could_not_connect() {
        let (_dir, path, served) = holder("error: no answer within 5s\n");
        let err = answer(&path, "server-a", Family::Any)
            .expect("the holder knew it")
            .unwrap_err();
        assert_eq!(err.to_string(), "no answer within 5s");
        served.join().unwrap();
    }

    #[test]
    fn nobody_listening_is_told_from_a_holder_that_does_not_know_the_host() {
        let dir = socket_dir();
        let path = dir.path().join("tailnet.sock");
        assert!(matches!(
            ask(
                &path,
                "server-a",
                22,
                Family::Any,
                Duration::from_secs(5),
                &|| false
            ),
            Asked::Nobody
        ));
    }

    /// An answer slower than a look still arrives whole: on Windows the
    /// asker waits between reads rather than having them time out.
    #[test]
    fn a_slow_answer_is_read_whole() {
        let dir = socket_dir();
        let path = dir.path().join("tailnet.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let served = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut byte = [0u8; 1];
            while stream.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {}
            for part in [&b"err"[..], b"or: slo", b"w\n"] {
                std::thread::sleep(LOOK * 3);
                stream.write_all(part).unwrap();
            }
        });
        let err = answer(&path, "server-a", Family::Any)
            .expect("the holder knew it")
            .unwrap_err();
        assert_eq!(err.to_string(), "slow");
        served.join().unwrap();
    }
}
