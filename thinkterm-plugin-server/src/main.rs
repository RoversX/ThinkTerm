//! The plugin host: ThinkTerm's plugins, in a process of their own.
//!
//! A client starts it when it first needs a plugin and nothing answers at
//! the socket (see thinkterm-plugin-channel); it exits by itself once nothing has
//! been connected for a while. One runs at a time: it holds a lock for as
//! long as it runs, and a second one gives way.
//!
//! The plugins built into it run inside it. An installed plugin runs as a
//! program of its own, which the host starts when the plugin is first used
//! -- or at once, for one that runs always while ThinkTerm keeps the host --
//! and stops once it has gone unused for long enough, or when the host
//! exits (see registry and docs/thinkterm/plugins.md).

// Started in the background, never from a console: no window of its own.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod builtins;
mod host;
mod manifest;
mod process;
mod registry;
mod snippets;
mod stamp;
mod switches;

use anyhow::Context;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thinkterm_plugin_channel::paths;
use wezterm_uds::UnixListener;

/// How long the host stays up with nobody connected to it.
const IDLE_EXIT: Duration = Duration::from_secs(30);
/// How long a host waits for the lock before leaving it to the one that
/// holds it: long enough for a host that was asked to quit to be gone.
const LOCK_WAIT: Duration = Duration::from_secs(3);
/// How long an installed plugin's program has to say it is ready.
const READY_WITHIN: Duration = Duration::from_secs(10);

struct Args {
    socket: PathBuf,
    lock: PathBuf,
    data_dir: PathBuf,
    idle: Duration,
    ready_within: Duration,
    /// How long programs run unused; a test's are shorter.
    limits: registry::Limits,
    /// Serve this built-in plugin over standard input and output, as an
    /// installed plugin's program does, instead of being the host.
    serve_plugin: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        socket: paths::socket(),
        lock: paths::lock(),
        data_dir: paths::data_dir(),
        idle: IDLE_EXIT,
        ready_within: READY_WITHIN,
        limits: registry::Limits::default(),
        serve_plugin: None,
    };
    let seconds = |name: &str, value: &std::ffi::OsStr| {
        value
            .to_str()
            .and_then(|secs| secs.parse().ok())
            .map(Duration::from_secs)
            .ok_or_else(|| format!("{name} needs a number of seconds"))
    };
    let mut given = std::env::args_os().skip(1);
    while let Some(arg) = given.next() {
        let name = arg.to_string_lossy().into_owned();
        let value = given
            .next()
            .ok_or_else(|| format!("{name} needs a value"))?;
        match name.as_str() {
            "--socket" => args.socket = value.into(),
            "--lock" => args.lock = value.into(),
            "--data-dir" => args.data_dir = value.into(),
            "--idle-secs" => args.idle = seconds(&name, &value)?,
            "--ready-secs" => args.ready_within = seconds(&name, &value)?,
            "--briefly-secs" => args.limits.briefly = seconds(&name, &value)?,
            "--never-secs" => args.limits.never = seconds(&name, &value)?,
            "--pending-secs" => args.limits.pending = seconds(&name, &value)?,
            "--serve-plugin" => args.serve_plugin = Some(value.to_string_lossy().into_owned()),
            _ => return Err(format!("unknown argument {name}")),
        }
    }
    Ok(args)
}

/// Standard error, which the client that started the host points at the
/// host's log.
struct Stderr;

impl log::Log for Stderr {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or(0);
            eprintln!(
                "{secs} {} [{}] {}",
                std::process::id(),
                record.level(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

static LOGGER: Stderr = Stderr;

fn main() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("thinkterm-plugin-server: {err}");
            std::process::exit(2);
        }
    };
    if let Some(id) = &args.serve_plugin {
        let Some(plugin) = builtins::one(id, &args.data_dir) else {
            eprintln!("thinkterm-plugin-server: there is no built-in plugin {id:?}");
            std::process::exit(2);
        };
        if let Err(err) = thinkterm_plugin_sdk::run(plugin) {
            log::error!("serving {id}: {err}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(err) = run(args) {
        log::error!("{err:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let Some(_lock) = take_lock(&args.lock)? else {
        log::info!("another plugin host is running; leaving it be");
        return Ok(());
    };
    // Nothing else listens at the socket while this holds the lock: a file
    // there was left by a host that died.
    match fs::remove_file(&args.socket) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("remove {}", args.socket.display())),
    }
    let listener = UnixListener::bind(&args.socket)
        .with_context(|| format!("listen at {}", args.socket.display()))?;
    make_private(&args.socket);
    log::info!("listening at {}", args.socket.display());

    let registry = registry::Registry::new(
        &args.data_dir,
        builtins::all(&args.data_dir),
        args.ready_within,
        args.limits,
    );
    let host = host::Host::new(args.socket, args.idle, registry);
    host.exit_when_idle();
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => host.accept(stream),
            Err(err) => {
                log::warn!("accepting a client: {err}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    Ok(())
}

/// The lock that makes this the only host, waiting a little for one that is
/// on its way out; `None` when another host keeps it.
fn take_lock(path: &Path) -> anyhow::Result<Option<File>> {
    if let Some(dir) = path.parent() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(dir)
            .with_context(|| format!("create {}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(err)) => {
                return Err(err).with_context(|| format!("lock {}", path.display()))
            }
        }
    }
}

/// Only this user may connect. The directory is private already; this is
/// for a socket placed somewhere that is not.
fn make_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
            log::warn!("making {} private: {err}", path.display());
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}
