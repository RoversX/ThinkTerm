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
        anyhow::bail!("unable to lock pid file {}: {}", pid_file.display(), err);
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
