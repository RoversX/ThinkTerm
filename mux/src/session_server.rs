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

/// Windows has no `flock`; the file's sharing mode is the lock. The server
/// keeps the pid file open with write sharing denied (`lock_pid_file`), and
/// the kernel drops that the moment its last handle closes -- when it exits
/// or dies -- exactly as `flock` does. A probe that asks for write access
/// is refused with ERROR_SHARING_VIOLATION while the server lives.
///
/// Unlike `flock`, any process can hold a file open that way (an antivirus
/// scanner, an indexer), so the pid read from the file is only believed if
/// it names a live `thinkterm-mux-server`.
#[cfg(windows)]
pub fn pid_holding(pid_file: &Path) -> Option<u32> {
    if !windows::pid_file_held(pid_file) {
        return None;
    }
    let pid = std::fs::read_to_string(pid_file)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 1)?;
    windows::is_mux_server_process(pid).then_some(pid)
}

/// The lock the Windows server holds for its lifetime: the pid file opened
/// with write sharing denied. Sibling of `daemonize::lock_pid_file` on unix.
/// Truncated on success; the caller writes its pid with `write_pid`.
///
/// A scanner holding the file for a moment looks like a holder too, so a
/// refusal is retried for about a second unless the pid in the file names a
/// live server, in which case it is refused at once with the unix wording.
#[cfg(windows)]
pub fn lock_pid_file(pid_file: &Path) -> anyhow::Result<std::fs::File> {
    windows::lock_pid_file(pid_file)
}

/// Write this process's pid into a locked pid file, replacing whatever the
/// launcher left there.
#[cfg(windows)]
pub fn write_pid(file: &mut std::fs::File) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{}", std::process::id())?;
    file.flush()
}

#[cfg(not(any(unix, windows)))]
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

/// Deliberately not `wezterm_uds::someone_listens`, which the mux server
/// uses to decide whether it may bind over a socket. That question fails
/// safe towards "someone is there"; this one asks whether *our* server is
/// up and fails safe towards "it is not", as the unix version above does on
/// every error. Sharing one probe would invert one of them.
#[cfg(windows)]
pub fn socket_answers(path: &Path) -> bool {
    wezterm_uds::UnixStream::connect(path).is_ok()
}

#[cfg(not(any(unix, windows)))]
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

/// Stop the server holding `pid_file`: its stop event is the SIGTERM of
/// this platform (the server flushes its layouts and exits on it), waiting
/// up to `wait` for its socket to close. Never `TerminateProcess`: a pid
/// can be recycled between the read and the kill, and every Windows server
/// that writes a pid file also creates the event, before the pid.
#[cfg(windows)]
pub fn stop(pid_file: &Path, socket: &Path, wait: Duration) -> anyhow::Result<StopOutcome> {
    let Some(pid) = pid_holding(pid_file) else {
        return Ok(StopOutcome::NotRunning);
    };
    windows::signal_stop(pid)?;
    let started = Instant::now();
    while started.elapsed() < wait {
        if !socket_answers(socket) && pid_holding(pid_file).is_none() {
            return Ok(StopOutcome::Stopped { pid });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(StopOutcome::Lingering { pid })
}

#[cfg(not(any(unix, windows)))]
pub fn stop(_pid_file: &Path, _socket: &Path, _wait: Duration) -> anyhow::Result<StopOutcome> {
    anyhow::bail!("stopping the session server is not supported on this platform")
}

/// The stop event a Windows server waits on, created by the server before
/// it publishes its pid and set by whoever stops it. Re-exported so the
/// server binary and the stoppers agree on one name.
#[cfg(windows)]
pub use windows::{create_stop_event, StopEvent};

#[cfg(windows)]
mod windows {
    use anyhow::Context as _;
    use std::os::windows::ffi::OsStringExt as _;
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::path::Path;
    use std::time::{Duration, Instant};
    use winapi::shared::winerror::ERROR_SHARING_VIOLATION;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::synchapi::{CreateEventW, OpenEventW, SetEvent, WaitForSingleObject};
    use winapi::um::winbase::{QueryFullProcessImageNameW, INFINITE};
    use winapi::um::winnt::{
        EVENT_MODIFY_STATE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, HANDLE,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    const SHARE_EVERYTHING: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    /// The image name a pid must carry to be believed as the server.
    const SERVER_IMAGE_STEM: &str = "thinkterm-mux-server";

    fn is_sharing_violation(err: &std::io::Error) -> bool {
        err.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32)
    }

    /// Whether some process holds `pid_file` open with write sharing denied.
    /// A file that does not exist, or any other failure, counts as not held.
    pub(super) fn pid_file_held(pid_file: &Path) -> bool {
        match std::fs::OpenOptions::new()
            .write(true)
            .share_mode(SHARE_EVERYTHING)
            .open(pid_file)
        {
            Ok(_) => false,
            Err(err) => is_sharing_violation(&err),
        }
    }

    /// Whether `pid` is a live process whose image is the mux server.
    pub(super) fn is_mux_server_process(pid: u32) -> bool {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        let mut buf = vec![0u16; 32 * 1024];
        let mut len = buf.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return false;
        }
        let path = std::path::PathBuf::from(std::ffi::OsString::from_wide(&buf[..len as usize]));
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.eq_ignore_ascii_case(SERVER_IMAGE_STEM))
    }

    fn holder_pid(pid_file: &Path) -> Option<u32> {
        std::fs::read_to_string(pid_file)
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid > 1)
    }

    pub(super) fn lock_pid_file(pid_file: &Path) -> anyhow::Result<std::fs::File> {
        let dir = pid_file
            .parent()
            .ok_or_else(|| anyhow::anyhow!("{} has no parent?", pid_file.display()))?;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("while creating directory structure: {}", dir.display()))?;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .share_mode(FILE_SHARE_READ)
                .open(pid_file)
            {
                Ok(file) => {
                    file.set_len(0)
                        .with_context(|| format!("truncating pid file {}", pid_file.display()))?;
                    return Ok(file);
                }
                Err(err) if is_sharing_violation(&err) => {
                    let holder = holder_pid(pid_file).filter(|pid| is_mux_server_process(*pid));
                    if holder.is_none() && Instant::now() < deadline {
                        // Something other than a server has it open for a
                        // moment; a scanner lets go, a server never does.
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                    let holder = holder
                        .map(|pid| format!(" (a mux server with pid {pid} holds it)"))
                        .unwrap_or_default();
                    anyhow::bail!("unable to lock pid file {}{holder}: {err}", pid_file.display());
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("opening pid file {}", pid_file.display()));
                }
            }
        }
    }

    fn stop_event_name(pid: u32) -> Vec<u16> {
        format!("Local\\ThinkTermMuxServer-{pid}-stop")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Set the stop event of the server with `pid`. An event that cannot
    /// be opened means no server of ours has that pid (it exited, and the
    /// number may already belong to someone else): an error, never a kill.
    pub(super) fn signal_stop(pid: u32) -> anyhow::Result<()> {
        let name = stop_event_name(pid);
        let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
        if handle.is_null() {
            anyhow::bail!(
                "could not stop the session server (pid {pid}): its stop event is not there: {}",
                std::io::Error::last_os_error()
            );
        }
        let ok = unsafe { SetEvent(handle) };
        let err = std::io::Error::last_os_error();
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            anyhow::bail!("could not stop the session server (pid {pid}): {err}");
        }
        Ok(())
    }

    /// The server's end of the stop event: owned for the life of the process.
    pub struct StopEvent(HANDLE);

    // A HANDLE is a plain kernel object reference; the waiting thread is the
    // only user after creation.
    unsafe impl Send for StopEvent {}

    impl StopEvent {
        /// Block until someone sets the event.
        pub fn wait(&self) {
            unsafe { WaitForSingleObject(self.0, INFINITE) };
        }
    }

    impl Drop for StopEvent {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Create this process's stop event. Manual reset, so a set that
    /// arrives before the waiter thread exists is not lost.
    pub fn create_stop_event() -> anyhow::Result<StopEvent> {
        let name = stop_event_name(std::process::id());
        let handle = unsafe { CreateEventW(std::ptr::null_mut(), 1, 0, name.as_ptr()) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error()).context("creating the stop event");
        }
        Ok(StopEvent(handle))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The sharing probe reports "held" only while a lock handle is open,
        /// which is what stands in for flock on this platform.
        #[test]
        fn a_pid_file_counts_only_while_locked() {
            let dir = std::env::temp_dir();
            let path = dir.join(format!("thinkterm-pid-lock-test-{}", std::process::id()));
            let _ = std::fs::remove_file(&path);
            assert!(!pid_file_held(&path), "a missing file is not held");

            let mut holder = lock_pid_file(&path).unwrap();
            super::super::write_pid(&mut holder).unwrap();
            assert!(pid_file_held(&path), "a locked file is held");
            assert_eq!(holder_pid(&path), Some(std::process::id()));
            // The test binary is not the server, so the public probe does not
            // believe the pid even though the lock is real.
            assert_eq!(super::super::pid_holding(&path), None);
            assert!(
                lock_pid_file(&path).is_err(),
                "a second lock is refused while the first is held"
            );

            drop(holder);
            assert!(!pid_file_held(&path), "dropping the handle releases the lock");
            assert_eq!(super::super::pid_holding(&path), None);
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn a_stop_event_is_found_by_pid_and_wakes_the_waiter() {
            let event = create_stop_event().unwrap();
            let waiter = std::thread::spawn(move || {
                event.wait();
            });
            signal_stop(std::process::id()).unwrap();
            waiter.join().unwrap();
            // Nothing of ours has this pid.
            assert!(signal_stop(u32::MAX - 7).is_err());
        }
    }
}
