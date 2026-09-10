//! Stopping the session server at the default socket: the one thing the
//! desktop, the CLI and the settings switch all do to it, in one place.
//! The server is found through the pid file it holds locked, never by
//! name, so a stale file or somebody else's process is left alone.

use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// Nothing holds the pid file: no server to stop.
    NotRunning,
    /// Asked to stop, and its socket went away within the wait.
    Stopped { pid: u32 },
    /// Asked to stop; its socket still answered when the wait ran out.
    Lingering { pid: u32 },
}

/// The pid of the server holding `pid_file`'s lock, if one does.
#[cfg(unix)]
pub fn pid_holding(pid_file: &Path) -> Option<u32> {
    use std::os::unix::io::AsRawFd as _;
    let file = std::fs::File::open(pid_file).ok()?;
    let locked_by_someone =
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0;
    if !locked_by_someone {
        // We hold it now: no server does. The lock goes with the file.
        return None;
    }
    std::fs::read_to_string(pid_file)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 1)
}

#[cfg(not(unix))]
pub fn pid_holding(_pid_file: &Path) -> Option<u32> {
    None
}

/// Whether something accepts on the unix socket at `path`: a non-blocking
/// connect, so a full backlog cannot hang the caller.
#[cfg(unix)]
pub fn socket_answers(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        return false;
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return false;
    }
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let rc = unsafe {
        libc::connect(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    let err = std::io::Error::last_os_error();
    unsafe { libc::close(fd) };
    rc == 0
        || matches!(
            err.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EAGAIN) | Some(libc::EWOULDBLOCK)
        )
}

#[cfg(not(unix))]
pub fn socket_answers(_path: &Path) -> bool {
    false
}

/// Whether a server holds the pid file and answers on the socket.
pub fn is_running(pid_file: &Path, socket: &Path) -> bool {
    pid_holding(pid_file).is_some() && socket_answers(socket)
}

/// Stop the server holding `pid_file` (SIGTERM: it ends its panes and
/// exits), waiting up to `wait` for its socket to close.
#[cfg(unix)]
pub fn stop(pid_file: &Path, socket: &Path, wait: Duration) -> anyhow::Result<StopOutcome> {
    let Some(pid) = pid_holding(pid_file) else {
        return Ok(StopOutcome::NotRunning);
    };
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
        anyhow::bail!(
            "could not stop the session server (pid {pid}): {}",
            std::io::Error::last_os_error()
        );
    }
    let started = Instant::now();
    while started.elapsed() < wait {
        if !socket_answers(socket) && pid_holding(pid_file).is_none() {
            return Ok(StopOutcome::Stopped { pid });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(StopOutcome::Lingering { pid })
}

#[cfg(not(unix))]
pub fn stop(_pid_file: &Path, _socket: &Path, _wait: Duration) -> anyhow::Result<StopOutcome> {
    anyhow::bail!("stopping the session server is not supported on this platform")
}
