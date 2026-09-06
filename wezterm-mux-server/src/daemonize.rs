#![cfg(unix)]
use anyhow::Context;
use libc::pid_t;
use std::io::Write;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};

enum Fork {
    #[allow(dead_code)]
    Child(pid_t),
    Parent(pid_t),
}

fn fork() -> anyhow::Result<Fork> {
    let pid = unsafe { libc::fork() };

    if pid == 0 {
        // We are the child
        let pid = unsafe { libc::getpid() };
        Ok(Fork::Child(pid))
    } else if pid < 0 {
        let err: anyhow::Error = std::io::Error::last_os_error().into();
        Err(err.context("fork"))
    } else {
        // We are the parent
        Ok(Fork::Parent(pid))
    }
}

fn setsid() -> anyhow::Result<()> {
    let pid = unsafe { libc::setsid() };
    if pid == -1 {
        let err: anyhow::Error = std::io::Error::last_os_error().into();
        Err(err.context("setsid"))
    } else {
        Ok(())
    }
}

pub fn lock_pid_file(config: &config::ConfigHandle) -> anyhow::Result<std::fs::File> {
    let pid_file = config.daemon_options.pid_file();
    let pid_file_dir = pid_file
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent?", pid_file.display()))?;
    std::fs::create_dir_all(&pid_file_dir).with_context(|| {
        format!(
            "while creating directory structure: {}",
            pid_file_dir.display()
        )
    })?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&pid_file)
        .with_context(|| format!("opening pid file {}", pid_file.display()))?;
    config::set_sticky_bit(&pid_file);
    let res = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if res != 0 {
        let err = std::io::Error::last_os_error();
        let holder = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .map(|pid| format!(" (a mux server with pid {pid} holds it)"))
            .unwrap_or_default();
        anyhow::bail!(
            "unable to lock pid file {}{holder}: {}",
            pid_file.display(),
            err
        );
    }

    unsafe { libc::ftruncate(file.as_raw_fd(), 0) };

    Ok(file)
}

/// What the daemon inherits from the process that launched it.
pub struct Daemonized {
    /// The locked pid file, to keep open for the life of the daemon.
    pub pid_file_fd: Option<RawFd>,
    /// The write end of the report pipe: the daemon writes one line to
    /// say how the takeover went, and the launching process relays it.
    pub report_fd: Option<RawFd>,
}

/// `lock_pid` is false for a server that is taking over from a running
/// one: that one holds the lock, and hands the locked file over with
/// everything else. With `report`, the process that ran the command does
/// not exit until the daemon has said how the takeover went, and exits
/// the way it went: forking twice would otherwise report success before
/// anything had happened.
pub fn daemonize(
    config: &config::ConfigHandle,
    lock_pid: bool,
    report: bool,
) -> anyhow::Result<Daemonized> {
    let report_pipe = if report {
        let mut fds = [0 as RawFd; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("pipe for the takeover report");
        }
        Some((fds[0], fds[1]))
    } else {
        None
    };
    let pid_file = if lock_pid && !config::running_under_wsl() {
        // pid file locking is only partly functional when running under
        // WSL 1; it is possible for the pid file to exist after a reboot
        // and for attempts to open and lock it to fail when there are no
        // other processes that might possibly hold a lock on it.
        // So, we only use a pid file when not under WSL.

        Some(lock_pid_file(config)?)
    } else {
        None
    };
    let stdout = config.daemon_options.open_stdout()?;
    let stderr = config.daemon_options.open_stderr()?;
    let devnull = std::fs::File::open("/dev/null").context("opening /dev/null for read")?;

    match fork()? {
        Fork::Parent(pid) => {
            let mut status = 0;
            unsafe { libc::waitpid(pid, &mut status, 0) };
            if let Some((read_fd, write_fd)) = report_pipe {
                unsafe { libc::close(write_fd) };
                std::process::exit(relay_takeover_report(read_fd));
            }
            std::process::exit(0);
        }
        Fork::Child(_) => {}
    }

    setsid()?;
    match fork()? {
        Fork::Parent(_) => {
            std::process::exit(0);
        }
        Fork::Child(_) => {}
    }

    let pid_file_fd = pid_file.map(|mut pid_file| {
        writeln!(pid_file, "{}", unsafe { libc::getpid() }).ok();
        // Leak it so that the descriptor remains open for the duration
        // of the process runtime
        let fd = pid_file.into_raw_fd();

        // Since we will always re-exec, we need to clear FD_CLOEXEC
        // in order for the pidfile to be inherited in our newly
        // exec'd self
        set_cloexec(fd, false);

        fd
    });

    unsafe { libc::dup2(devnull.as_raw_fd(), libc::STDIN_FILENO) };
    unsafe { libc::dup2(stdout.as_raw_fd(), libc::STDOUT_FILENO) };
    unsafe { libc::dup2(stderr.as_raw_fd(), libc::STDERR_FILENO) };

    let report_fd = report_pipe.map(|(read_fd, write_fd)| {
        unsafe { libc::close(read_fd) };
        set_cloexec(write_fd, false);
        write_fd
    });

    Ok(Daemonized {
        pid_file_fd,
        report_fd,
    })
}

/// Wait for the daemon's one-line report and turn it into an exit status:
/// `ok` is 0 and silent, `error: <why>` prints why and is 1, nothing at
/// all (the daemon died first) is 1 too.
fn relay_takeover_report(read_fd: RawFd) -> i32 {
    use std::io::Read;
    let mut report = String::new();
    let mut pipe = unsafe { std::fs::File::from_raw_fd(read_fd) };
    pipe.read_to_string(&mut report).ok();
    let line = report.lines().next().unwrap_or("").trim();
    if line == "ok" {
        return 0;
    }
    if let Some(why) = line.strip_prefix("error: ") {
        eprintln!("takeover failed: {why}");
    } else {
        eprintln!("the takeover did not report an outcome; see the server log");
    }
    1
}

pub fn set_cloexec(fd: RawFd, enable: bool) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags == -1 {
            return;
        }

        let flags = if enable {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };

        libc::fcntl(fd, libc::F_SETFD, flags);
    }
}

/// Replace this process with `program`, making the new image responsible
/// for itself in macOS privacy (TCC) terms.
///
/// A daemon spawned by the GUI is otherwise attributed to that GUI: the
/// first GUI's grant covers it, and once that GUI has exited the attribution
/// dangles, so a Documents/Desktop Project is refused and the shell lands in
/// `$HOME` "sometimes". Disclaiming gives the server one stable identity of
/// its own, granted once. Only returns on failure, like `exec`.
#[cfg(target_os = "macos")]
pub fn exec_as_own_tcc_identity(
    program: &std::ffi::OsStr,
    args: &[std::ffi::OsString],
) -> std::io::Error {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    type SetDisclaim = unsafe extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int;
    let symbol = unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            b"responsibility_spawnattrs_setdisclaim\0".as_ptr() as *const libc::c_char,
        )
    };
    if symbol.is_null() {
        return std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "responsibility_spawnattrs_setdisclaim is not available",
        );
    }
    let set_disclaim: SetDisclaim = unsafe { std::mem::transmute(symbol) };

    let c_program = match CString::new(program.as_bytes()) {
        Ok(program) => program,
        Err(err) => return std::io::Error::new(std::io::ErrorKind::InvalidInput, err),
    };
    let c_args: Vec<CString> = std::iter::once(program)
        .chain(args.iter().map(|arg| arg.as_os_str()))
        .filter_map(|arg| CString::new(arg.as_bytes()).ok())
        .collect();
    let c_env: Vec<CString> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let mut entry = key.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            CString::new(entry).ok()
        })
        .collect();
    let mut argv: Vec<*mut libc::c_char> =
        c_args.iter().map(|arg| arg.as_ptr() as *mut _).collect();
    argv.push(std::ptr::null_mut());
    let mut envp: Vec<*mut libc::c_char> =
        c_env.iter().map(|entry| entry.as_ptr() as *mut _).collect();
    envp.push(std::ptr::null_mut());

    unsafe {
        let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
        let rc = libc::posix_spawnattr_init(&mut attr);
        if rc != 0 {
            return std::io::Error::from_raw_os_error(rc);
        }
        // SETEXEC turns posix_spawn into exec-in-place; the disclaim applies
        // to the replaced image.
        let rc = libc::posix_spawnattr_setflags(&mut attr, libc::POSIX_SPAWN_SETEXEC as libc::c_short);
        if rc != 0 {
            libc::posix_spawnattr_destroy(&mut attr);
            return std::io::Error::from_raw_os_error(rc);
        }
        let rc = set_disclaim(&mut attr, 1);
        if rc != 0 {
            libc::posix_spawnattr_destroy(&mut attr);
            return std::io::Error::from_raw_os_error(rc);
        }
        let mut pid: libc::pid_t = 0;
        let rc = libc::posix_spawn(
            &mut pid,
            c_program.as_ptr(),
            std::ptr::null(),
            &attr,
            argv.as_ptr(),
            envp.as_ptr(),
        );
        libc::posix_spawnattr_destroy(&mut attr);
        std::io::Error::from_raw_os_error(rc)
    }
}
