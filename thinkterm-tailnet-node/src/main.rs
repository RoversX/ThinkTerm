//! `thinkterm-tailnet`: the machine's node in the tailnet, in a process of its
//! own. The mux server starts it while the settings turn the tailnet on (see
//! `thinkterm_tailnet::keep_node`), and it holds the node the way the server
//! once did itself (see `thinkterm_tailnet::follow`): by the node's lock, one
//! process at a time, answering the machine's other processes on the
//! tailnet's socket and writing the report they read.
//!
//! It ends with the process that started it, and once the settings turn the
//! tailnet off; the server starts it again when the tailnet is on.

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
use std::time::Duration;

/// How often it looks whether to end.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
const WATCH: Duration = Duration::from_secs(1);

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn main() {
    log_to_file();
    let parent = match parse_parent() {
        Ok(parent) => parent,
        Err(err) => {
            log::error!("{err}");
            eprintln!("thinkterm-tailnet: {err}");
            std::process::exit(2);
        }
    };
    #[cfg(windows)]
    let watched = windows::Parent::watch(parent);
    log::info!("holding the tailnet's node for process {parent}");
    thinkterm_tailnet::follow(|| {});
    loop {
        // Already gone by the time this looks is gone all the same: the
        // process is handed to another parent as its own ends.
        #[cfg(unix)]
        let gone = std::os::unix::process::parent_id() != parent;
        // Windows hands it to nobody: a handle on the process tells instead.
        #[cfg(windows)]
        let gone = watched.has_gone();
        if gone {
            log::info!("process {parent}, which started this, has gone");
            break;
        }
        if thinkterm_tailnet::is_off() {
            log::info!("the tailnet is off");
            break;
        }
        // Ended for good: nothing acts on the settings any more. Started
        // anew by the mux server, it does again.
        if !thinkterm_tailnet::is_following() {
            log::error!("the thread that follows the settings has ended");
            std::process::exit(1);
        }
        std::thread::sleep(WATCH);
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn main() {
    eprintln!("thinkterm-tailnet: joining a tailnet is not supported on this platform");
    std::process::exit(2);
}

/// The process it ends with: `--parent <pid>`, which the mux server passes,
/// else whichever started it.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn parse_parent() -> Result<u32, String> {
    let mut args = std::env::args().skip(1);
    let mut parent = started_by();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--parent" => {
                parent = args
                    .next()
                    .and_then(|pid| pid.parse().ok())
                    .ok_or("--parent needs a process id")?;
            }
            _ => return Err(format!("unknown argument {arg:?}")),
        }
    }
    Ok(parent)
}

/// The process that started this one.
#[cfg(unix)]
fn started_by() -> u32 {
    std::os::unix::process::parent_id()
}

/// The process that started this one, by the number Windows keeps for it
/// even after it has gone; 0, which names no process to watch, when that
/// cannot be had.
#[cfg(windows)]
fn started_by() -> u32 {
    windows::parent_id().unwrap_or(0)
}

/// The log: a file of its own in the runtime folder, named as the mux
/// server's are, so that the app prunes it with theirs once it is a week
/// old. Readable by this user alone: a login link may be in it. On Windows
/// it takes the folder's permissions, which under the user's profile are
/// this user's, SYSTEM's and the Administrators'. Standard error when the
/// file cannot be had. Panics go to it too.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
fn log_to_file() {
    use std::io::Write;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Log(Mutex<Box<dyn Write + Send>>);

    impl log::Log for Log {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.level() <= log::Level::Info
        }

        fn log(&self, record: &log::Record) {
            if !self.enabled(record.metadata()) {
                return;
            }
            let since = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let line = format!(
                "{}.{:03} [{}] {}: {}\n",
                since.as_secs(),
                since.subsec_millis(),
                record.level(),
                record.target(),
                record.args()
            );
            if let Ok(mut out) = self.0.lock() {
                let _ = out.write_all(line.as_bytes());
            }
        }

        fn flush(&self) {}
    }

    let dir = &*config::RUNTIME_DIR;
    let path = dir.join(format!("thinkterm-tailnet-log-{}.txt", std::process::id()));
    let out: Box<dyn Write + Send> = match std::fs::create_dir_all(dir).and_then(|()| {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&path)
    }) {
        Ok(file) => Box::new(file),
        Err(err) => {
            eprintln!("thinkterm-tailnet: logging to {}: {err}", path.display());
            Box::new(std::io::stderr())
        }
    };
    if log::set_boxed_logger(Box::new(Log(Mutex::new(out)))).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    std::panic::set_hook(Box::new(|info| {
        log::error!(
            "panic: {info}\n{}",
            std::backtrace::Backtrace::force_capture()
        );
    }));
}

/// The process it ends with, on Windows, which keeps a process's parent's
/// number after the parent has gone -- for a new process to be given, in
/// time -- rather than hand the process to another parent.
#[cfg(windows)]
mod windows {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

    /// The process this one ends with, held by a handle: that goes on
    /// meaning it, whatever its number is given to later. None when it had
    /// gone already, or another process had its number by then.
    pub struct Parent(Option<OwnedHandle>);

    impl Parent {
        pub fn watch(pid: u32) -> Self {
            Self(open(pid))
        }

        pub fn has_gone(&self) -> bool {
            use winapi::shared::winerror::WAIT_TIMEOUT;
            use winapi::um::synchapi::WaitForSingleObject;
            match &self.0 {
                Some(process) => unsafe {
                    WaitForSingleObject(process.as_raw_handle() as _, 0) != WAIT_TIMEOUT
                },
                None => true,
            }
        }
    }

    /// Process `pid`, if it is the one that started this: a parent starts
    /// before its child, so one that started after this one has the number
    /// of one that has gone.
    fn open(pid: u32) -> Option<OwnedHandle> {
        use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcess};
        use winapi::um::winnt::{PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZE};
        let handle =
            unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            log::warn!(
                "cannot watch process {pid}: {}",
                std::io::Error::last_os_error()
            );
            return None;
        }
        let process = unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) };
        let own = started(unsafe { GetCurrentProcess() } as RawHandle)?;
        match started(process.as_raw_handle()) {
            Some(started) if started <= own => Some(process),
            _ => None,
        }
    }

    /// When a process started, in Windows' 100ns ticks.
    fn started(process: RawHandle) -> Option<u64> {
        use winapi::shared::minwindef::FILETIME;
        use winapi::um::processthreadsapi::GetProcessTimes;
        let zero = || FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero(), zero(), zero(), zero());
        let ok = unsafe {
            GetProcessTimes(
                process as _,
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        };
        (ok != 0)
            .then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }

    /// The number of the process that started this one.
    pub fn parent_id() -> Option<u32> {
        use winapi::um::handleapi::INVALID_HANDLE_VALUE;
        use winapi::um::tlhelp32::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot as RawHandle) };
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let me = std::process::id();
        let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle() as _, &mut entry) } != 0;
        while more {
            if entry.th32ProcessID == me {
                return Some(entry.th32ParentProcessID);
            }
            more = unsafe { Process32NextW(snapshot.as_raw_handle() as _, &mut entry) } != 0;
        }
        None
    }
}
