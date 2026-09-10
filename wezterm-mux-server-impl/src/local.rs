use anyhow::{anyhow, Context as _};
use config::{create_user_owned_dirs, UnixDomain};
use wezterm_uds::UnixListener;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};

#[cfg(unix)]
lazy_static::lazy_static! {
    /// A duplicate of the listening socket of each unix domain, by path,
    /// so a handoff can pass it on to the successor. A duplicate, not the
    /// number: should the accept loop end and close its own, the number
    /// would come to name whatever was opened next.
    static ref LISTENER_FDS: std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, std::os::fd::OwnedFd>> =
        std::sync::Mutex::new(std::collections::HashMap::new());
}

/// The descriptor of the listener bound at `socket_path`, if this server
/// bound one; valid for as long as the registry holds it.
#[cfg(unix)]
pub fn listener_fd_for(socket_path: &std::path::Path) -> Option<std::os::unix::io::RawFd> {
    use std::os::fd::AsRawFd;
    LISTENER_FDS
        .lock()
        .unwrap()
        .get(socket_path)
        .map(|fd| fd.as_raw_fd())
}

/// Remember the listener at `socket_path`, for a later handoff.
#[cfg(unix)]
pub fn remember_listener(socket_path: std::path::PathBuf, listener: &UnixListener) {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::io::AsRawFd;
    let dup = unsafe { libc::dup(listener.as_raw_fd()) };
    if dup < 0 {
        log::error!(
            "cannot duplicate the listener at {} for a later handoff: {}",
            socket_path.display(),
            std::io::Error::last_os_error()
        );
        return;
    }
    unsafe { libc::fcntl(dup, libc::F_SETFD, libc::FD_CLOEXEC) };
    LISTENER_FDS
        .lock()
        .unwrap()
        .insert(socket_path, unsafe { OwnedFd::from_raw_fd(dup) });
}

pub struct LocalListener {
    listener: UnixListener,
}

impl LocalListener {
    pub fn new(listener: UnixListener) -> Self {
        Self { listener }
    }

    pub fn with_domain(unix_dom: &UnixDomain) -> anyhow::Result<Self> {
        let listener = safely_create_sock_path(unix_dom)?;
        #[cfg(unix)]
        remember_listener(unix_dom.socket_path(), &listener);
        Ok(Self::new(listener))
    }

    pub fn run(&mut self) {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    crate::connections::spawn(async move {
                        if let Err(err) = crate::dispatch::process(
                            stream,
                            crate::sessionhandler::ConnectionPeer::Local,
                        )
                        .await
                        {
                            log::error!("{err:#}");
                        }
                    });
                }
                Err(err) => {
                    log::error!("accept failed: {}", err);
                    return;
                }
            }
        }
    }
}

/// Make `path` free to bind: remove the socket file a previous server
/// left there, unless a server still answers on it. A server that is not
/// the daemon -- one run in the foreground, say by mistaking
/// `thinkterm-mux-server cli list` for the client -- takes no pid lock,
/// and without this it would bind the path out from under the running one:
/// that one keeps serving its connections on an inode nothing can reach,
/// and every later `thinkterm cli` is refused.
pub fn claim_socket_path(path: &std::path::Path) -> anyhow::Result<()> {
    if someone_listens(path) {
        anyhow::bail!(
            "a mux server is already listening on {}; refusing to bind over it. \
             `thinkterm cli list` talks to it; `thinkterm-mux-server --daemonize --takeover` \
             replaces it",
            path.display()
        );
    }
    // On windows, we can't tell if the unix domain socket exists using the
    // methods on Path, so we just unconditionally remove it and see what
    // error occurs.
    match std::fs::remove_file(path) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).context(format!("Unable to remove {}", path.display())),
    }
}

/// `claim_socket_path` for a server: besides the probe, the pid file is
/// consulted. On macOS a listener whose accept backlog is full answers a
/// non-blocking connect with ECONNREFUSED, the same as a stale file, so a
/// server stalled long enough to fill its backlog would read as absent
/// and be bound over -- the very thing the probe exists to prevent. A pid
/// file held under lock by a live process other than this one says a
/// server is there whatever the probe found.
pub fn claim_socket_path_for_server(
    path: &std::path::Path,
    pid_file: &std::path::Path,
) -> anyhow::Result<()> {
    if let Some(pid) = other_server_holding_pid_file(pid_file) {
        anyhow::bail!(
            "a mux server (pid {pid}) holds {}; refusing to bind {} over it. \
             `thinkterm cli list` talks to it; `thinkterm-mux-server --daemonize --takeover` \
             replaces it",
            pid_file.display(),
            path.display()
        );
    }
    claim_socket_path(path)
}

/// The pid in `pid_file` when a live process other than this one holds
/// the file's lock.
#[cfg(unix)]
fn other_server_holding_pid_file(pid_file: &std::path::Path) -> Option<u32> {
    use std::os::unix::io::AsRawFd as _;
    let file = std::fs::File::open(pid_file).ok()?;
    // Our own lock (the daemon locks before it binds) conflicts with this
    // probe too, so the pid decides whether the holder is us.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return None;
    }
    let pid = std::fs::read_to_string(pid_file)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    if pid <= 1 || pid == std::process::id() {
        return None;
    }
    (unsafe { libc::kill(pid as libc::pid_t, 0) } == 0).then_some(pid)
}

/// No answer off unix: the pid file is written by `daemonize`, which is
/// `#![cfg(unix)]` entire, so on Windows there is nothing holding a lock to
/// ask about. `someone_listens` carries the whole guard there, which leaves
/// a server stalled long enough to fill its accept backlog able to be bound
/// over -- the case this pid file exists to catch on unix.
#[cfg(not(unix))]
fn other_server_holding_pid_file(_pid_file: &std::path::Path) -> Option<u32> {
    None
}

/// Whether a connect to `path` reaches a listener. Only a missing file
/// and a refused connection mean nobody does; everything else -- a
/// connection, EAGAIN from a full backlog, or any other failure -- counts
/// as someone, because binding over a live server is the one outcome
/// this must never allow. Non-blocking, so a full backlog cannot hang
/// the caller.
#[cfg(unix)]
pub fn someone_listens(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        // Cannot be probed (nor bound): let the bind report it.
        return true;
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return true;
    }
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    unsafe {
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let len = std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1;
    let res = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            len as libc::socklen_t,
        )
    };
    if res == 0 {
        return true;
    }
    !matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ENOENT) | Some(libc::ECONNREFUSED)
    )
}

/// Windows has real `AF_UNIX`, so the same question has a real answer; see
/// [`wezterm_uds::someone_listens`] for what its error codes can and cannot
/// distinguish.
#[cfg(windows)]
pub fn someone_listens(path: &std::path::Path) -> bool {
    wezterm_uds::someone_listens(path)
}

#[cfg(not(any(unix, windows)))]
pub fn someone_listens(_path: &std::path::Path) -> bool {
    false
}

/// Take care when setting up the listener socket;
/// we need to be sure that the directory that we create it in
/// is owned by the user and has appropriate file permissions
/// that prevent other users from manipulating its contents.
fn safely_create_sock_path(unix_dom: &UnixDomain) -> anyhow::Result<UnixListener> {
    let sock_path = &unix_dom.socket_path();
    log::trace!("setting up {}", sock_path.display());

    let sock_dir = sock_path
        .parent()
        .ok_or_else(|| anyhow!("sock_path {} has no parent dir", sock_path.display()))?;

    create_user_owned_dirs(sock_dir)?;

    #[cfg(unix)]
    {
        use config::running_under_wsl;
        use std::os::unix::fs::PermissionsExt;

        if !running_under_wsl() && !unix_dom.skip_permissions_check {
            // Let's be sure that the ownership looks sane
            let meta = sock_dir.symlink_metadata()?;

            let permissions = meta.permissions();
            if (permissions.mode() & 0o22) != 0 {
                anyhow::bail!(
                    "The permissions for {} are insecure and currently \
                     allow other users to write to it (permissions={:?})",
                    sock_dir.display(),
                    permissions
                );
            }
        }
    }

    // The pid file belongs to the daemon serving the profile's socket; a
    // GUI's own per-process listener (`gui-sock-<pid>`) must not be refused
    // because a daemon is running.
    let daemon_socket = sock_path.file_name() == Some(std::ffi::OsStr::new(&config::runtime_file_name("sock")));
    if daemon_socket {
        claim_socket_path_for_server(sock_path, &config::configuration().daemon_options.pid_file())?;
    } else {
        claim_socket_path(sock_path)?;
    }

    let listener = UnixListener::bind(sock_path)
        .with_context(|| format!("Failed to bind to {}", sock_path.display()))?;

    config::set_sticky_bit(&sock_path);

    Ok(listener)
}

#[cfg(all(test, unix))]
mod tests {
    use super::claim_socket_path;

    fn short_dir() -> tempfile::TempDir {
        // Socket paths are short; the default temp dir on macOS is not.
        tempfile::Builder::new()
            .prefix("tt-")
            .tempdir_in("/tmp")
            .unwrap()
    }

    #[test]
    fn a_path_with_a_listener_is_refused() {
        let dir = short_dir();
        let path = dir.path().join("sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let err = claim_socket_path(&path).unwrap_err();
        assert!(
            err.to_string().contains("already listening"),
            "{err:#}"
        );
        assert!(path.exists(), "the live socket file must stay");
    }

    #[test]
    fn a_stale_socket_file_is_removed() {
        let dir = short_dir();
        let path = dir.path().join("sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        claim_socket_path(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn a_missing_file_is_free() {
        let dir = short_dir();
        claim_socket_path(&dir.path().join("sock")).unwrap();
    }
}
