use clap::*;
use config::configuration;
use mux::activity::Activity;
use mux::domain::{Domain, LocalDomain};
use mux::Mux;
use portable_pty::cmdbuilder::CommandBuilder;
use std::ffi::OsString;
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use wezterm_gui_subcommands::*;
use wezterm_mux_server_impl::update_mux_domains_for_server;

mod daemonize;
mod stats;

#[derive(Debug, Parser)]
#[command(
    about = "ThinkTerm Multiplexer Server\nhttps://github.com/RoversX/thinkterm",
    version = config::wezterm_version(),
    trailing_var_arg = true,
)]
struct Opt {
    /// Skip loading the ThinkTerm configuration
    #[arg(long, short = 'n')]
    skip_config: bool,

    /// Specify the configuration file to use, overrides the normal
    /// configuration file resolution
    #[arg(
        long,
        value_parser,
        conflicts_with = "skip_config",
        value_hint=ValueHint::FilePath,
    )]
    config_file: Option<OsString>,

    /// Override specific configuration values
    #[arg(
        long = "config",
        name = "name=value",
        value_parser=clap::builder::ValueParser::new(name_equals_value),
        number_of_values = 1)]
    config_override: Vec<(String, String)>,

    /// Detach from the foreground and become a background process
    #[arg(long = "daemonize")]
    daemonize: bool,

    /// Take over from the mux server already running on this socket:
    /// its panes, windows and clients continue under this binary, and
    /// the running server exits. The running server must support it.
    #[cfg(unix)]
    #[arg(long = "takeover")]
    takeover: bool,

    /// Start with no pane at all instead of one running the default
    /// program: for a server whose panes are all spawned by a client,
    /// such as the GUI keeping its local terminals in it.
    #[arg(long = "no-initial-pane")]
    no_initial_pane: bool,

    /// Specify the current working directory for the initially
    /// spawned program
    #[arg(long = "cwd", value_parser, value_hint=ValueHint::DirPath)]
    cwd: Option<OsString>,

    #[cfg(unix)]
    #[arg(long, hide = true)]
    pid_file_fd: Option<i32>,

    /// The pipe a daemonized takeover reports its outcome on; see
    /// daemonize::Daemonized.
    #[cfg(unix)]
    #[arg(long, hide = true)]
    takeover_report_fd: Option<i32>,

    /// The pid file the launching `--daemonize` process locked and made
    /// inheritable, as a raw handle value; this process writes its pid into
    /// it and keeps it open for its lifetime. The Windows sibling of
    /// `--pid-file-fd`.
    #[cfg(windows)]
    #[arg(long, hide = true)]
    pid_file_handle: Option<usize>,

    /// Instead of executing your shell, run PROG.
    /// For example: `thinkterm start -- bash -l` will spawn bash
    /// as if it were a login shell.
    #[arg(value_parser, value_hint=ValueHint::CommandWithArguments, num_args=1..)]
    prog: Vec<OsString>,
}

fn main() {
    if let Err(err) = run() {
        wezterm_blob_leases::clear_storage();
        log::error!("{:#}", err);
        #[cfg(unix)]
        wezterm_mux_server_impl::handoff::report_takeover(Err(&format!("{err:#}")));
        std::process::exit(1);
    }
    wezterm_blob_leases::clear_storage();
}

fn run() -> anyhow::Result<()> {
    env_bootstrap::bootstrap();

    //stats::Stats::init()?;
    config::designate_this_as_the_main_thread();
    let _saver = umask::UmaskSaver::new();

    let opts = Opt::parse();

    #[cfg(unix)]
    {
        // Ensure that we set CLOEXEC on the inherited lock file
        // before we have an opportunity to spawn any child processes.
        if let Some(fd) = opts.pid_file_fd {
            daemonize::set_cloexec(fd, true);
        }
        if let Some(fd) = opts.takeover_report_fd {
            daemonize::set_cloexec(fd, true);
            wezterm_mux_server_impl::handoff::remember_report_fd(fd);
        }
    }

    #[cfg(windows)]
    windows_daemon::adopt(opts.pid_file_handle)?;

    config::common_init(
        opts.config_file.as_ref(),
        &opts.config_override,
        opts.skip_config,
    )?;
    config::ignore_native_settings_changes();

    let config = config::configuration();

    #[cfg(windows)]
    windows_daemon::lock_own_pid_file(&config)?;

    config.update_ulimit()?;
    if let Some(value) = &config.default_ssh_auth_sock {
        std::env::set_var("SSH_AUTH_SOCK", value);
    }

    #[cfg(unix)]
    let mut pid_file = None;
    #[cfg(unix)]
    let mut report_fd = None;

    #[cfg(unix)]
    {
        if opts.daemonize {
            let daemonized = daemonize::daemonize(&config, !opts.takeover, opts.takeover)?;
            pid_file = daemonized.pid_file_fd;
            report_fd = daemonized.report_fd;
            // When we reach this line, we are in a forked child process,
            // and the fork will have broken the async-io/reactor state
            // of the smol runtime.
            // To resolve this, we will re-exec ourselves in the block
            // below that was originally Windows-specific
        }
    }

    if opts.daemonize {
        // On Windows we can't literally daemonize, but we can spawn another copy
        // of ourselves in the background!
        // On Unix, forking breaks the global state maintained by `smol`,
        // so we need to re-exec ourselves to start things back up properly.
        let mut cmd = Command::new(std::env::current_exe().unwrap());

        #[cfg(unix)]
        {
            // Inform the new version of ourselves that we already
            // locked the pidfile so that it can prevent it from
            // being propagated to its children when they spawn
            if let Some(fd) = pid_file {
                cmd.arg("--pid-file-fd");
                cmd.arg(&fd.to_string());
            }
            if let Some(fd) = report_fd {
                cmd.arg("--takeover-report-fd");
                cmd.arg(&fd.to_string());
            }
        }
        if opts.skip_config {
            cmd.arg("-n");
        }
        #[cfg(unix)]
        if opts.takeover {
            cmd.arg("--takeover");
        }
        if opts.no_initial_pane {
            cmd.arg("--no-initial-pane");
        }
        if let Some(f) = &opts.config_file {
            cmd.arg("--config-file");
            cmd.arg(f);
        }
        for (name, value) in &opts.config_override {
            cmd.arg("--config");
            cmd.arg(&format!("{name}={value}"));
        }
        if let Some(cwd) = opts.cwd {
            cmd.arg("--cwd");
            cmd.arg(cwd);
        }
        if !opts.prog.is_empty() {
            cmd.arg("--");
            for a in &opts.prog {
                cmd.arg(a);
            }
        }

        #[cfg(windows)]
        {
            return windows_daemon::spawn_detached(cmd, &config);
        }

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            if let Some(mask) = umask::UmaskSaver::saved_umask() {
                unsafe {
                    cmd.pre_exec(move || {
                        libc::umask(mask);
                        Ok(())
                    });
                }
            }

            #[cfg(target_os = "macos")]
            {
                // The umask pre_exec above only runs through `exec`; apply
                // it in place, since a disclaiming exec keeps this process.
                if let Some(mask) = umask::UmaskSaver::saved_umask() {
                    unsafe { libc::umask(mask) };
                }
                // A disclaiming exec of a binary macOS has not assessed (an
                // unnotarized build) stalls in dyld for most of a minute, and
                // the GUI gives up on the socket long before; developers opt
                // out and keep the launcher's identity.
                let opted_out = std::env::var_os("THINKTERM_NO_PRIVACY_DISCLAIM")
                    .is_some_and(|value| !value.is_empty());
                if opted_out {
                    log::info!("THINKTERM_NO_PRIVACY_DISCLAIM is set; keeping the launcher's privacy identity");
                } else {
                    let args: Vec<OsString> =
                        cmd.get_args().map(|arg| arg.to_os_string()).collect();
                    let err = daemonize::exec_as_own_tcc_identity(cmd.get_program(), &args);
                    log::warn!("re-exec with own privacy identity failed, using exec: {err:#}");
                }
            }

            return Err(anyhow::anyhow!("failed to re-exec: {:?}", cmd.exec()));
        }
    }

    // Capture what we inherited before either removal loop runs: the mux
    // server strips SSH_AUTH_SOCK from its own environment on purpose, and
    // AgentProxy (constructed with the Mux below) needs the value to publish
    // the agent.PID symlink every pane is pointed at.
    mux::ssh_agent::stash_inherited_ssh_auth_sock();

    // Remove some environment variables that aren't super helpful or
    // that are potentially misleading when we're starting up the
    // server.
    // We may potentially want to look into starting/registering
    // a session of some kind here as well in the future.
    for name in &[
        "OLDPWD",
        "PWD",
        "SHLVL",
        "WEZTERM_PANE",
        "WEZTERM_UNIX_SOCKET",
        "_",
    ] {
        std::env::remove_var(name);
    }
    for name in &config::configuration().mux_env_remove {
        std::env::remove_var(name);
    }

    wezterm_blob_leases::register_storage(Arc::new(
        wezterm_blob_leases::simple_tempdir::SimpleTempDir::new_in(&*config::CACHE_DIR)?,
    ))?;

    let no_initial_pane = opts.no_initial_pane;
    // Only the desktop launches the server this way; its tree is then the
    // desktop's to seed, never this server's.
    wezterm_mux_server_impl::thinkterm_tree::set_hosted_by_desktop(no_initial_pane);
    let need_builder = !opts.prog.is_empty() || opts.cwd.is_some();

    let cmd = if need_builder {
        let mut builder = if opts.prog.is_empty() {
            CommandBuilder::new_default_prog()
        } else {
            CommandBuilder::from_argv(opts.prog)
        };
        if let Some(cwd) = opts.cwd {
            builder.cwd(cwd);
        }
        Some(builder)
    } else {
        None
    };

    let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);

    // A takeover learns who the running server is before the mux exists:
    // the runtime server id and the id counters are inherited.
    #[cfg(unix)]
    let takeover = if opts.takeover {
        let socket_path = takeover_socket_path(&config)?;
        let takeover = wezterm_mux_server_impl::handoff::begin(&socket_path)?;
        let hello = &takeover.hello;
        log::info!(
            "taking over from mux server {} ({} panes left behind)",
            hello.runtime_server_id,
            hello.left_behind.len()
        );
        mux::pane::reserve_pane_ids_below(hello.next_pane_id);
        mux::tab::reserve_tab_ids_below(hello.next_tab_id);
        mux::tab::reserve_pane_stack_ids_below(hello.next_stack_id);
        mux::window::reserve_window_ids_below(hello.next_window_id);
        Some(takeover)
    } else {
        None
    };
    #[cfg(not(unix))]
    let takeover: Option<()> = None;

    #[cfg(unix)]
    let mux = match &takeover {
        Some(takeover) => Arc::new(mux::Mux::new_with_runtime_server_id(
            Some(domain.clone()),
            takeover.hello.runtime_server_id.clone(),
        )),
        None => Arc::new(mux::Mux::new(Some(domain.clone()))),
    };
    #[cfg(not(unix))]
    let mux = Arc::new(mux::Mux::new(Some(domain.clone())));
    Mux::set_mux(&mux);
    // Before the listener: a client registering earlier than the stored
    // mode is loaded gets the default one and never accepts the fix-up.
    wezterm_mux_server_impl::thinkterm_access::initialize_mux(&mux);

    install_shutdown_signal_handler()?;

    stats::init_from_env()?;

    let executor = promise::spawn::SimpleExecutor::new();

    // The listener lives in this binary; the session handler that answers a
    // settings window lives in the impl crate, which cannot call up into
    // this one, so the controls are handed down here.
    //
    // Ahead of the branch below, and not inside `spawn_listener`, because
    // only the non-takeover path calls that. A server started with
    // `--takeover` -- which is what an in-place update does -- bound the
    // port from its own thread and then had no controls at all: the
    // settings switch read Off while browsers were connecting, pressing it
    // said there was no listener to turn off, and "Copy link" minted a
    // token, got no URL back and revoked the token it had just made.
    wezterm_mux_server_impl::web_control::install(
        wezterm_mux_server_impl::web_control::WebControl {
            start: web::spawn_web_listener,
            configure_tokens: web::configure_tokens,
            stop: web::stop_web_listener,
            listening: web::listening,
            effective: web::effective,
            certificates: web::certificates,
        },
    );

    if takeover.is_none() {
        spawn_listener().map_err(|e| {
            log::error!("problem spawning listeners: {:?}", e);
            e
        })?;
    } else {
        // The listener arrives with the handoff; the environment is set
        // now for the same reason spawn_listener sets it early.
        if let Some(unix_dom) = config.unix_domains.last() {
            std::env::set_var("WEZTERM_UNIX_SOCKET", unix_dom.socket_path());
        }
    }

    #[cfg(unix)]
    if let Some(fd) = opts.pid_file_fd {
        wezterm_mux_server_impl::handoff::remember_pid_file_fd(fd);
    }
    // Past daemonizing: its thread is the one this process keeps.
    wezterm_mux_server_impl::keep_plugin_host_up();

    let activity = Activity::new();

    let daemonized = opts.daemonize;
    promise::spawn::spawn(async move {
        if let Err(err) = async_run(cmd, takeover, daemonized, no_initial_pane).await {
            terminate_with_error(err);
        }
        drop(activity);
    })
    .detach();

    loop {
        executor.tick()?;
    }
}

async fn trigger_mux_startup(lua: Option<Rc<mlua::Lua>>) -> anyhow::Result<()> {
    if let Some(lua) = lua {
        let args = lua.pack_multi(())?;
        config::lua::emit_event(&lua, ("mux-startup".to_string(), args)).await?;
    }
    Ok(())
}

#[cfg(unix)]
type TakeoverHandle = wezterm_mux_server_impl::handoff::Takeover;
#[cfg(not(unix))]
type TakeoverHandle = ();

/// The socket a takeover connects to: the one WEZTERM_UNIX_SOCKET names.
#[cfg(unix)]
fn takeover_socket_path(config: &config::ConfigHandle) -> anyhow::Result<std::path::PathBuf> {
    config
        .unix_domains
        .last()
        .map(|unix_dom| unix_dom.socket_path())
        .ok_or_else(|| anyhow::anyhow!("no unix domain is configured, nothing to take over"))
}

async fn async_run(
    cmd: Option<CommandBuilder>,
    takeover: Option<TakeoverHandle>,
    daemonized: bool,
    no_initial_pane: bool,
) -> anyhow::Result<()> {
    let mux = Mux::get();
    let config = config::configuration();
    let took_over = takeover.is_some();

    // async_run is entered through SimpleExecutor, so promise's schedulers
    // are available before agent detection starts its safety tick.
    mux::agent_status::initialize_mux(&mux);
    mux::foreground_program::initialize_mux(&mux);
    update_mux_domains_for_server(&config)?;
    mux::spawn_idle_image_sweeper();

    #[cfg(unix)]
    if let Some(mut takeover) = takeover {
        // The pid file outlives the takeover handle: it stays open, locked,
        // for the life of this process.
        let pid_file_fd = takeover.pid_file_fd.take().map(|fd| {
            use std::os::unix::io::IntoRawFd;
            fd.into_raw_fd()
        });
        let local_domain = mux.default_domain().domain_id();
        let socket_path = takeover_socket_path(&config)?;
        let stream = match wezterm_mux_server_impl::handoff::complete(
            takeover,
            local_domain,
            &socket_path,
        ) {
            Ok(stream) => stream,
            Err(err) => {
                // Nothing of ours is registered and the layout files are
                // the running server's: leave without the exit flush.
                log::error!("taking over from the running server failed: {err:#}");
                wezterm_mux_server_impl::handoff::report_takeover(Err(&format!("{err:#}")));
                std::process::exit(2);
            }
        };
        wezterm_mux_server_impl::handoff::report_takeover(Ok(()));
        match pid_file_fd {
            Some(fd) => adopt_pid_file(fd),
            // The predecessor ran in the foreground and had no pid file to
            // hand over; a daemon takes one of its own, or the next
            // `--daemonize` would bind the socket out from under it.
            None if daemonized => match daemonize::lock_pid_file(&config) {
                Ok(mut file) => {
                    use std::io::Write;
                    writeln!(file, "{}", std::process::id()).ok();
                    std::mem::forget(file);
                }
                Err(err) => log::warn!("no pid file after the takeover: {err:#}"),
            },
            None => {}
        }
        // The handoff listener and the TCP listeners wait for the old
        // server to exit: it still answers on the handoff path and holds
        // the ports until then, and a path someone answers on is not bound
        // over. Everything is ours already; not listening for the next
        // successor is worth a log line, not an exit.
        let tls_servers = config.tls_servers.clone();
        let web_servers = config.web_servers.clone();
        thread::spawn(move || {
            wezterm_mux_server_impl::handoff::wait_for_predecessor_exit(stream);
            // Its descriptors close one after another as it exits; the
            // handoff socket may answer for a while after the stream did.
            // A server that cannot be taken over is one whose sessions the
            // next update ends, so this keeps trying for a long time
            // rather than giving up at once.
            let handoff_path = wezterm_mux_server_impl::handoff::handoff_socket_path(&socket_path);
            let give_up = std::time::Instant::now() + std::time::Duration::from_secs(600);
            let mut last_err = None;
            loop {
                if !wezterm_mux_server_impl::local::someone_listens(&handoff_path) {
                    match wezterm_mux_server_impl::handoff::spawn_handoff_listener(
                        socket_path.clone(),
                    ) {
                        Ok(()) => break,
                        Err(err) => last_err = Some(err),
                    }
                }
                if std::time::Instant::now() >= give_up {
                    log::error!(
                        "not listening for a successor: {}",
                        last_err
                            .map(|err| format!("{err:#}"))
                            .unwrap_or_else(|| "the predecessor's handoff socket kept answering".into())
                    );
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(250));
            }
            for tls_server in &tls_servers {
                if let Err(err) = ossl::spawn_tls_listener(tls_server) {
                    log::error!("problem spawning TLS listener after the takeover: {err:#}");
                }
            }
            if let Err(err) = web::configure_tokens(&web_servers) {
                log::error!("problem loading web tokens after the takeover: {err:#}");
            }
            for web_server in &web_servers {
                if let Err(err) = web::spawn_web_listener(web_server) {
                    log::error!("problem spawning web listener after the takeover: {err:#}");
                }
            }
        });
    }
    #[cfg(not(unix))]
    let _ = (takeover, daemonized);
    let _config_subscription = config::subscribe_to_config_reload(move || {
        promise::spawn::spawn_into_main_thread(async move {
            if let Err(err) = update_mux_domains_for_server(&config::configuration()) {
                log::error!("Error updating mux domains: {:#}", err);
            }
            // The reloaded config may name a different color_scheme; every
            // connection is told, so browsers repaint with it.
            Mux::get().notify(mux::MuxNotification::DefaultPaletteChanged);
        })
        .detach();
        true
    });

    let domain = mux.default_domain();

    // A takeover continues a mux that started long ago; its startup event
    // already ran there.
    if !took_over {
        if let Err(err) = config::with_lua_config_on_main_thread(trigger_mux_startup).await {
            log::error!("while processing mux-startup event: {:#}", err);
        }
    }

    let have_panes_in_domain = mux
        .iter_panes()
        .iter()
        .any(|p| p.domain_id() == domain.domain_id());

    if !have_panes_in_domain && !no_initial_pane {
        let workspace = None;
        let position = None;
        let window_id = mux.new_empty_window(workspace, position);
        domain.attach(Some(*window_id)).await?;

        let _tab = mux
            .default_domain()
            .spawn(config.initial_size(0, None), cmd, None, *window_id)
            .await?;
    }
    Ok(())
}

/// The pid file the predecessor handed over: same lock, our pid in it.
#[cfg(unix)]
fn adopt_pid_file(fd: std::os::unix::io::RawFd) {
    use std::io::{Seek, SeekFrom, Write};
    daemonize::set_cloexec(fd, true);
    wezterm_mux_server_impl::handoff::remember_pid_file_fd(fd);
    let mut file = std::mem::ManuallyDrop::new(unsafe {
        use std::os::unix::io::FromRawFd;
        std::fs::File::from_raw_fd(fd)
    });
    // The offset is shared with the predecessor's writes: back to the
    // start, or the pid lands after a hole of NULs.
    if let Err(err) = file
        .set_len(0)
        .and_then(|_| file.seek(SeekFrom::Start(0)))
        .and_then(|_| writeln!(file, "{}", std::process::id()))
    {
        log::warn!("rewriting the pid file after the takeover: {err:#}");
    }
}

fn terminate_with_error(err: anyhow::Error) -> ! {
    log::error!("{:#}; terminating", err);
    if let Err(flush_err) = wezterm_mux_server_impl::thinkterm_layout::flush_now() {
        log::error!("flushing ThinkTerm layouts before termination: {flush_err:#}");
    }
    std::process::exit(1);
}

#[cfg(unix)]
fn install_shutdown_signal_handler() -> anyhow::Result<()> {
    use signal_hook::consts::signal::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            promise::spawn::spawn_into_main_thread(async move {
                if let Err(err) = wezterm_mux_server_impl::thinkterm_layout::flush_now() {
                    log::error!("flushing ThinkTerm layouts before shutdown: {err:#}");
                }
                std::process::exit(0);
            })
            .detach();
        }
    });
    Ok(())
}

/// The Windows sibling of the SIGTERM thread above: the stop event created
/// in `windows_daemon::adopt` (before the pid was published, so a stopper
/// that reads the pid always finds it) and Ctrl+C on a foreground server
/// both flush the layouts and exit.
#[cfg(windows)]
fn install_shutdown_signal_handler() -> anyhow::Result<()> {
    windows_daemon::install_shutdown_handler()
}

#[cfg(not(any(unix, windows)))]
fn install_shutdown_signal_handler() -> anyhow::Result<()> {
    Ok(())
}

/// What `daemonize.rs` does on unix, for Windows: the launching process
/// locks the pid file and hands it to a detached child, the child publishes
/// its pid and answers a named stop event instead of SIGTERM. There is no
/// takeover: an in-place update passes descriptors, which this platform
/// cannot, so a running server keeps serving until it is stopped.
#[cfg(windows)]
mod windows_daemon {
    use anyhow::Context as _;
    use mux::session_server::StopEvent;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, RawHandle};
    use std::os::windows::process::CommandExt as _;
    use std::sync::Mutex;
    use winapi::shared::minwindef::{BOOL, DWORD, TRUE};
    use winapi::shared::winerror::ERROR_ACCESS_DENIED;
    use winapi::um::consoleapi::SetConsoleCtrlHandler;
    use winapi::um::handleapi::SetHandleInformation;
    use winapi::um::winbase::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, HANDLE_FLAG_INHERIT,
    };
    use winapi::um::winnt::HANDLE;

    /// Held from `adopt` until `install_shutdown_handler` takes it.
    static STOP_EVENT: Mutex<Option<StopEvent>> = Mutex::new(None);

    /// Runs first thing after argument parsing, in every server process:
    /// create the stop event, and when a launcher handed over the locked pid
    /// file, take it out of inheritance (shells spawned later must not carry
    /// it, as `set_cloexec` does on unix) and write this pid into it.
    pub fn adopt(pid_file_handle: Option<usize>) -> anyhow::Result<()> {
        let event = mux::session_server::create_stop_event()?;
        *STOP_EVENT.lock().unwrap() = Some(event);
        if let Some(raw) = pid_file_handle {
            let mut file = unsafe { std::fs::File::from_raw_handle(raw as RawHandle) };
            unsafe { SetHandleInformation(raw as HANDLE, HANDLE_FLAG_INHERIT, 0) };
            match mux::session_server::write_pid(&mut file) {
                Ok(()) => std::mem::forget(file),
                Err(err) => {
                    // The handle did not survive the spawn. The launcher is
                    // exiting and lets go of its lock; `lock_own_pid_file`
                    // takes one once the configuration says where it is.
                    log::warn!(
                        "the inherited pid file handle is unusable ({err:#}); \
                         the pid file is locked directly instead"
                    );
                    std::mem::forget(file);
                    NEEDS_OWN_LOCK.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        Ok(())
    }

    static NEEDS_OWN_LOCK: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// After the configuration is loaded: the fallback lock for a daemon
    /// whose inherited handle was unusable. The launcher's lock is released
    /// as it exits, which `lock_pid_file` waits out.
    pub fn lock_own_pid_file(config: &config::ConfigHandle) -> anyhow::Result<()> {
        if !NEEDS_OWN_LOCK.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let mut own = mux::session_server::lock_pid_file(&config.daemon_options.pid_file())?;
        mux::session_server::write_pid(&mut own)?;
        std::mem::forget(own);
        Ok(())
    }

    /// The `--daemonize` launcher: lock the pid file (a second launcher
    /// fails here with the unix wording), then start the server detached,
    /// with the locked file inherited. Returns once the child is running;
    /// the client connects to the socket with retries, as on unix.
    pub fn spawn_detached(
        mut cmd: std::process::Command,
        config: &config::ConfigHandle,
    ) -> anyhow::Result<()> {
        let pid_file = mux::session_server::lock_pid_file(&config.daemon_options.pid_file())?;
        let raw = pid_file.as_raw_handle();
        if unsafe { SetHandleInformation(raw as HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0
        {
            return Err(std::io::Error::last_os_error()).context("making the pid file inheritable");
        }
        cmd.arg("--pid-file-handle");
        cmd.arg((raw as usize).to_string());
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(config.daemon_options.open_stdout()?);
        cmd.stderr(config.daemon_options.open_stderr()?);

        // Detached from this console and from the launcher's job, so a GUI
        // started under a kill-on-close job (installers, some launchers)
        // does not take the server down with it.
        let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        cmd.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB);
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) if err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
                log::warn!(
                    "the launcher's job does not allow breaking away; the session server stays \
                     in it and ends with the job"
                );
                cmd.creation_flags(flags);
                cmd.spawn().context("spawning the session server")?
            }
            Err(err) => return Err(err).context("spawning the session server"),
        };
        log::info!("session server started as pid {}", child.id());
        // The child holds its own copy of the lock; this one closes with us.
        drop(child);
        drop(pid_file);
        Ok(())
    }

    unsafe extern "system" fn ctrl_handler(_ctrl_type: DWORD) -> BOOL {
        request_shutdown();
        TRUE
    }

    fn request_shutdown() {
        promise::spawn::spawn_into_main_thread(async move {
            if let Err(err) = wezterm_mux_server_impl::thinkterm_layout::flush_now() {
                log::error!("flushing ThinkTerm layouts before shutdown: {err:#}");
            }
            std::process::exit(0);
        })
        .detach();
    }

    pub fn install_shutdown_handler() -> anyhow::Result<()> {
        let event = STOP_EVENT
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow::anyhow!("the stop event was not created"))?;
        std::thread::Builder::new()
            .name("stop-event".into())
            .spawn(move || {
                event.wait();
                log::info!("stop requested; flushing and exiting");
                request_shutdown();
            })
            .context("spawning the stop-event thread")?;
        unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), TRUE) };
        Ok(())
    }
}

mod ossl;
mod web;

pub fn spawn_listener() -> anyhow::Result<()> {
    let config = configuration();
    // The environment is written before any thread that reads it exists:
    // connection threads read it for the version handshake, and setenv
    // against a concurrent getenv is not safe.
    if let Some(unix_dom) = config.unix_domains.last() {
        std::env::set_var("WEZTERM_UNIX_SOCKET", unix_dom.socket_path());
    }
    // Two domains naming one socket path would have the second bind over
    // the first (now refused): one listener per path.
    let mut bound = std::collections::HashSet::new();
    for unix_dom in &config.unix_domains {
        if !bound.insert(unix_dom.socket_path()) {
            log::warn!(
                "unix domain {} shares the socket path of an earlier domain; not binding it twice",
                unix_dom.name
            );
            continue;
        }
        let mut listener = wezterm_mux_server_impl::local::LocalListener::with_domain(unix_dom)?;
        thread::spawn(move || {
            listener.run();
        });
        // Not fatal: a server that cannot be taken over is still a server.
        #[cfg(unix)]
        if let Err(err) =
            wezterm_mux_server_impl::handoff::spawn_handoff_listener(unix_dom.socket_path())
        {
            log::error!("not listening for a successor: {err:#}");
        }
    }

    for tls_server in &config.tls_servers {
        ossl::spawn_tls_listener(tls_server)?;
    }

    // Not fatal: a token store that cannot be read means no web tokens,
    // not no server.
    if let Err(err) = web::configure_tokens(&config.web_servers) {
        log::error!("problem loading web tokens: {err:#}");
    }
    for web_server in &config.web_servers {
        web::spawn_web_listener(web_server)?;
    }

    Ok(())
}
