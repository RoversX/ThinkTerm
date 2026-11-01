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

    /// The pipe a daemonized takeover reports its outcome on, as a raw
    /// handle value: the Windows sibling of `--takeover-report-fd`.
    #[cfg(windows)]
    #[arg(long, hide = true)]
    takeover_report_handle: Option<usize>,

    /// Instead of executing your shell, run PROG.
    /// For example: `thinkterm start -- bash -l` will spawn bash
    /// as if it were a login shell.
    #[arg(value_parser, value_hint=ValueHint::CommandWithArguments, num_args=1..)]
    prog: Vec<OsString>,
}

fn main() {
    // A server that handed its panes over, staying on as the same process
    // only to wait: before anything else starts, as it serves nothing.
    #[cfg(target_os = "macos")]
    if let Some(watch) = wezterm_mux_server_impl::hold::requested() {
        wezterm_mux_server_impl::hold::run(&watch, env_bootstrap::setup_logger);
    }
    // The GUI asking what macOS lets the programs in panes do: answer with
    // the identity a server has, which the launcher's is not, and exit.
    #[cfg(target_os = "macos")]
    {
        use wezterm_mux_server_impl::privacy;
        match privacy::requested() {
            Some(privacy::Request::TakeIdentity) => {
                let err = match std::env::current_exe() {
                    Ok(exe) => daemonize::exec_as_own_tcc_identity(
                        exe.as_os_str(),
                        &[privacy::REPORT_AS_SELF_OPTION.into()],
                    ),
                    Err(err) => err,
                };
                eprintln!("privacy report: could not take the server's identity: {err:#}");
                std::process::exit(1);
            }
            Some(privacy::Request::Report) => {
                privacy::print_report();
                std::process::exit(0);
            }
            None => {}
        }
    }
    thinkterm_import_herdr::register().expect("register built-in session importer");
    if let Some(result) = thinkterm_import::maybe_run_helper() {
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }
    if let Err(err) = run() {
        wezterm_blob_leases::clear_storage();
        log::error!("{:#}", err);
        #[cfg(any(unix, windows))]
        let status = wezterm_mux_server_impl::handoff::report_takeover_failure(&err);
        #[cfg(not(any(unix, windows)))]
        let status = 1;
        std::process::exit(status);
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
    windows_daemon::adopt(opts.pid_file_handle, opts.takeover_report_handle)?;

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
            return windows_daemon::spawn_detached(cmd, &config, opts.takeover);
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

    let mux = match &takeover {
        Some(takeover) => Arc::new(mux::Mux::new_with_runtime_server_id(
            Some(domain.clone()),
            takeover.hello.runtime_server_id.clone(),
        )),
        None => Arc::new(mux::Mux::new(Some(domain.clone()))),
    };
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
        // The listener arrives with the handoff (on Windows it is bound once
        // the predecessor has exited); the environment is set now for the
        // same reason spawn_listener sets it early.
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

    // The detached child of a `--daemonize --takeover` launcher is the
    // daemon, though it is not started with --daemonize: the report pipe it
    // was given says so.
    #[cfg(windows)]
    let daemonized = opts.daemonize || opts.takeover_report_handle.is_some();
    #[cfg(not(windows))]
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

type TakeoverHandle = wezterm_mux_server_impl::handoff::Takeover;

/// The socket a takeover connects to: the one WEZTERM_UNIX_SOCKET names.
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
    // Before anything connects: a takeover's sessions reconnect at once.
    // Until the predecessor's node has gone with it, theirs to the tailnet's
    // machines wait for this server's own (see keep_tailnet_node).
    #[cfg(feature = "tailnet")]
    {
        // Through the node of whichever process here holds it.
        thinkterm_tailnet::use_for_ssh_in_server();
        if took_over {
            thinkterm_tailnet::will_hold();
        }
    }

    // async_run is entered through SimpleExecutor, so promise's schedulers
    // are available before agent detection starts its safety tick.
    mux::agent_status::initialize_mux(&mux);
    mux::foreground_program::initialize_mux(&mux);
    update_mux_domains_for_server(&config)?;
    mux::spawn_idle_image_sweeper();

    if let Some(mut takeover) = takeover {
        // The pid file outlives the takeover handle: it stays open, locked,
        // for the life of this process.
        let pid_file = takeover.pid_file.take();
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
        match pid_file {
            Some(file) => adopt_pid_file(file),
            // The predecessor ran in the foreground and had no pid file to
            // hand over; a daemon takes one of its own, or the next
            // `--daemonize` would bind the socket out from under it.
            None if daemonized => {
                if let Err(err) = lock_own_pid_file(&config) {
                    log::warn!("no pid file after the takeover: {err:#}");
                }
            }
            None => {}
        }
        // The report and the listeners wait for the old server to exit (or,
        // on macOS, to become a process that only waits): until then it may
        // still accept on the socket, and a client told "ok" that reconnects
        // at once could reach it and be refused as another build. It also
        // still answers on the handoff path and holds the ports, and a path
        // someone answers on is not bound over. Everything is ours already;
        // not listening for the next successor is worth a log line, not an
        // exit.
        let tls_servers = config.tls_servers.clone();
        let web_servers = config.web_servers.clone();
        thread::spawn(move || {
            let mut stream = stream;
            let let_go = wezterm_mux_server_impl::handoff::wait_for_predecessor_exit(
                &mut stream,
                Some(wezterm_mux_server_impl::handoff::LET_GO_TIMEOUT),
            );
            // The listening socket did not travel: it is free to bind only
            // once the old server is gone, and the report says "reconnect".
            #[cfg(windows)]
            {
                if !let_go {
                    log::warn!("the old server has not let go yet; waiting for it");
                    wezterm_mux_server_impl::handoff::wait_for_predecessor_exit(&mut stream, None);
                }
                listen_after_takeover();
            }
            wezterm_mux_server_impl::handoff::report_takeover(Ok(()));
            #[cfg(unix)]
            if !let_go {
                log::warn!("the old server has not let go yet; reported the takeover all the same");
                wezterm_mux_server_impl::handoff::wait_for_predecessor_exit(&mut stream, None);
            }
            // The predecessor's node goes with it, as its `thinkterm-tailnet`
            // ends with it: two processes on one node's keys would take the
            // node from each other.
            #[cfg(feature = "tailnet")]
            keep_tailnet_node();
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
    #[cfg(feature = "tailnet")]
    if !took_over {
        keep_tailnet_node();
    }
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

/// Keeps the machine's node running while the tailnet is on -- the app's
/// switch or `thinkterm tailnet up` (see thinkterm_tailnet) -- in
/// `thinkterm-tailnet`, a program of its own, which also takes SSH
/// connections from the tailnet to this machine's sshd when `thinkterm
/// tailnet up` said to.
#[cfg(feature = "tailnet")]
fn keep_tailnet_node() {
    thinkterm_tailnet::keep_node();
}

/// A pid file of this daemon's own, locked, with its pid in it.
#[cfg(unix)]
fn lock_own_pid_file(config: &config::ConfigHandle) -> anyhow::Result<()> {
    use std::io::Write;
    let mut file = daemonize::lock_pid_file(config)?;
    writeln!(file, "{}", std::process::id()).ok();
    std::mem::forget(file);
    Ok(())
}

#[cfg(windows)]
fn lock_own_pid_file(config: &config::ConfigHandle) -> anyhow::Result<()> {
    windows_daemon::take_own_pid_file(config)
}

/// The pid file the predecessor handed over: same lock, our pid in it.
#[cfg(windows)]
fn adopt_pid_file(handle: std::os::windows::io::OwnedHandle) {
    let mut file = std::fs::File::from(handle);
    if let Err(err) = mux::session_server::write_pid(&mut file) {
        log::warn!("rewriting the pid file after the takeover: {err:#}");
    }
    windows_daemon::keep_pid_file(file);
}

/// Bind the socket the predecessor listened on, now that it has exited:
/// on Windows the listening socket does not travel with the panes. This
/// server holds every pane by now, so it never gives up: past half a minute
/// the launcher hears of the failure, and the binding goes on being tried.
#[cfg(windows)]
fn listen_after_takeover() {
    let config = configuration();
    let Some(unix_dom) = config.unix_domains.last() else {
        wezterm_mux_server_impl::handoff::report_takeover(Err("no unix domain is configured"));
        return;
    };
    let started = std::time::Instant::now();
    let mut reported = false;
    loop {
        match wezterm_mux_server_impl::local::LocalListener::with_domain(unix_dom) {
            Ok(mut listener) => {
                thread::spawn(move || listener.run());
                if reported {
                    log::info!("listening on the socket after all");
                }
                return;
            }
            // Its socket closes a moment after its process is reported gone.
            Err(err) if !reported && started.elapsed() >= std::time::Duration::from_secs(30) => {
                log::error!("cannot listen on the socket yet; still trying: {err:#}");
                wezterm_mux_server_impl::handoff::report_takeover(Err(&format!(
                    "the panes were taken over, but the socket cannot be bound yet: {err:#}"
                )));
                reported = true;
            }
            Err(_) => {}
        }
        thread::sleep(std::time::Duration::from_millis(if reported { 1000 } else { 100 }));
    }
}

/// The pid file the predecessor handed over: same lock, our pid in it.
#[cfg(unix)]
fn adopt_pid_file(fd: std::os::unix::io::OwnedFd) {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::io::IntoRawFd;
    let fd = fd.into_raw_fd();
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
    wezterm_mux_server_impl::thinkterm_layout::keep_pane_output();
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
                wezterm_mux_server_impl::thinkterm_layout::keep_pane_output();
                wezterm_mux_server_impl::share::shut_down();
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
/// its pid and answers a named stop event instead of SIGTERM. A takeover's
/// launcher locks nothing -- the running server hands its pid file over --
/// and waits for the child's one-line report instead, as on unix.
#[cfg(windows)]
mod windows_daemon {
    use anyhow::Context as _;
    use mux::session_server::StopEvent;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, RawHandle};
    use std::os::windows::process::CommandExt as _;
    use std::sync::Mutex;
    use std::time::Duration;
    use winapi::shared::minwindef::{BOOL, DWORD, LPARAM, LRESULT, TRUE, UINT, WPARAM};
    use winapi::shared::windef::HWND;
    use winapi::shared::winerror::ERROR_ACCESS_DENIED;
    use winapi::um::consoleapi::SetConsoleCtrlHandler;
    use winapi::um::handleapi::{SetHandleInformation, INVALID_HANDLE_VALUE};
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::processenv::GetStdHandle;
    use winapi::um::processthreadsapi::SetProcessShutdownParameters;
    use winapi::um::winbase::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, HANDLE_FLAG_INHERIT,
        STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use winapi::um::winnt::HANDLE;
    use winapi::um::winuser::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassExW,
        TranslateMessage, MSG, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSEXW, WS_OVERLAPPED,
    };

    /// Windows ends the process as soon as the handlers below return; they
    /// wait this long for the main thread to keep what it keeps and exit on
    /// its own first.
    const EXIT_GRACE: Duration = Duration::from_secs(5);

    /// A logoff or shutdown ends processes a level at a time, highest first
    /// and in no set order within one. Every process starts at 0x280, the
    /// shells in the server's terminals among them: one level up, the server
    /// keeps what its panes show while they are still there. The desktop
    /// goes one further up, so it is gone before the server and never starts
    /// a new one to rebuild Threads into while the session ends.
    const SHUTDOWN_LEVEL: DWORD = 0x300;

    /// Held from `adopt` until `install_shutdown_handler` takes it.
    static STOP_EVENT: Mutex<Option<StopEvent>> = Mutex::new(None);

    /// Runs first thing after argument parsing, in every server process:
    /// create the stop event, and when a launcher handed over the locked pid
    /// file, take it out of inheritance (shells spawned later must not carry
    /// it, as `set_cloexec` does on unix) and write this pid into it. The
    /// pipe a takeover reports on is taken out of inheritance the same way.
    pub fn adopt(pid_file_handle: Option<usize>, report_handle: Option<usize>) -> anyhow::Result<()> {
        if let Some(raw) = report_handle {
            unsafe { SetHandleInformation(raw as HANDLE, HANDLE_FLAG_INHERIT, 0) };
            wezterm_mux_server_impl::handoff::remember_report_handle(raw as RawHandle);
        }
        let event = mux::session_server::create_stop_event()?;
        *STOP_EVENT.lock().unwrap() = Some(event);
        if let Some(raw) = pid_file_handle {
            let mut file = unsafe { std::fs::File::from_raw_handle(raw as RawHandle) };
            unsafe { SetHandleInformation(raw as HANDLE, HANDLE_FLAG_INHERIT, 0) };
            match mux::session_server::write_pid(&mut file) {
                Ok(()) => keep_pid_file(file),
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
        take_own_pid_file(config)
    }

    /// Lock the pid file directly and publish this pid in it.
    pub fn take_own_pid_file(config: &config::ConfigHandle) -> anyhow::Result<()> {
        let mut own = mux::session_server::lock_pid_file(&config.daemon_options.pid_file())?;
        mux::session_server::write_pid(&mut own)?;
        keep_pid_file(own);
        Ok(())
    }

    /// Hold the pid file -- the lock -- for the life of this process, and
    /// note it for a takeover to hand on.
    pub fn keep_pid_file(file: std::fs::File) {
        wezterm_mux_server_impl::handoff::remember_pid_file_handle(file.as_raw_handle());
        std::mem::forget(file);
    }

    /// The `--daemonize` launcher: lock the pid file (a second launcher
    /// fails here with the unix wording), then start the server detached,
    /// with the locked file inherited. Returns once the child is running;
    /// the client connects to the socket with retries, as on unix. For a
    /// takeover it waits for the child's report and exits with its outcome.
    pub fn spawn_detached(
        mut cmd: std::process::Command,
        config: &config::ConfigHandle,
        takeover: bool,
    ) -> anyhow::Result<()> {
        let pid_file = if takeover {
            None
        } else {
            let pid_file = mux::session_server::lock_pid_file(&config.daemon_options.pid_file())?;
            let raw = pid_file.as_raw_handle();
            if unsafe { SetHandleInformation(raw as HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
                == 0
            {
                return Err(std::io::Error::last_os_error())
                    .context("making the pid file inheritable");
            }
            cmd.arg("--pid-file-handle");
            cmd.arg((raw as usize).to_string());
            Some(pid_file)
        };
        let report = if takeover {
            let (read, write) = report_pipe()?;
            cmd.arg("--takeover-report-handle");
            cmd.arg((write.as_raw_handle() as usize).to_string());
            Some((read, write))
        } else {
            None
        };
        // This launcher's own stdio may be the pipes a client reads to the
        // end (a takeover's is): the server outlives it and must not hold
        // them. Its own stdio is the log, given below.
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = unsafe { GetStdHandle(which) };
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
            }
        }
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
        if let Some((read, write)) = report {
            // Only the child may hold the write end, or a child that died
            // without a word would never read as one.
            drop(write);
            std::process::exit(relay_takeover_report(read));
        }
        Ok(())
    }

    /// A pipe whose write end the child inherits.
    fn report_pipe() -> anyhow::Result<(std::fs::File, std::os::windows::io::OwnedHandle)> {
        use std::os::windows::io::OwnedHandle;
        use winapi::um::minwinbase::SECURITY_ATTRIBUTES;
        use winapi::um::namedpipeapi::CreatePipe;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: TRUE,
        };
        let mut read = std::ptr::null_mut();
        let mut write = std::ptr::null_mut();
        if unsafe { CreatePipe(&mut read, &mut write, &mut attributes, 0) } == 0 {
            return Err(std::io::Error::last_os_error()).context("creating the takeover report pipe");
        }
        unsafe { SetHandleInformation(read, HANDLE_FLAG_INHERIT, 0) };
        Ok((
            unsafe { std::fs::File::from_raw_handle(read as RawHandle) },
            unsafe { OwnedHandle::from_raw_handle(write as RawHandle) },
        ))
    }

    /// The daemon's one-line report as an exit status, as `daemonize.rs`
    /// reads it on unix: `ok` is 0, `refused: <why>` (it could not start at
    /// all) is `TAKEOVER_CANNOT_HAND_OVER`, anything else is 1.
    fn relay_takeover_report(mut pipe: std::fs::File) -> i32 {
        use std::io::Read;
        let mut report = String::new();
        pipe.read_to_string(&mut report).ok();
        let line = report.lines().next().unwrap_or("").trim();
        if line == "ok" {
            return 0;
        }
        if let Some(why) = line.strip_prefix("refused: ") {
            eprintln!("takeover failed: {why}");
            return mux::session_server::TAKEOVER_CANNOT_HAND_OVER;
        }
        if let Some(why) = line.strip_prefix("error: ") {
            eprintln!("takeover failed: {why}");
        } else {
            eprintln!("the takeover did not report an outcome; see the server log");
        }
        1
    }

    unsafe extern "system" fn ctrl_handler(_ctrl_type: DWORD) -> BOOL {
        shut_down_before_returning();
        TRUE
    }

    /// Windows announces a logoff or shutdown to a process only through its
    /// top-level windows: a detached server has no console for the handler
    /// above, and a console process that is not a service is not sent those
    /// events either. So a window nobody sees waits for the announcement, on
    /// a thread of its own.
    fn watch_session_end() -> anyhow::Result<()> {
        std::thread::Builder::new()
            .name("session-end".into())
            .spawn(|| {
                if let Err(err) = run_session_end_window() {
                    log::warn!("not watching for logoff and shutdown: {err:#}");
                }
            })
            .context("spawning the session-end thread")?;
        Ok(())
    }

    fn run_session_end_window() -> anyhow::Result<()> {
        let wide =
            |text: &str| -> Vec<u16> { OsStr::new(text).encode_wide().chain(Some(0)).collect() };
        let name = wide("ThinkTermSessionEnd");
        // Shown should Windows list the server among the programs still
        // closing.
        let title = wide("ThinkTerm session server");
        // SAFETY: `class` and the strings outlive the calls that read them,
        // the procedure has the signature Windows calls it with, and the
        // window lives on this thread, which runs its message loop until the
        // process ends.
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as UINT,
                lpfnWndProc: Some(session_end_proc),
                hInstance: instance,
                lpszClassName: name.as_ptr(),
                ..std::mem::zeroed()
            };
            if RegisterClassExW(&class) == 0 {
                anyhow::bail!(
                    "registering the window class: {}",
                    std::io::Error::last_os_error()
                );
            }
            // Top-level and never shown: a message-only window is not told.
            let window = CreateWindowExW(
                0,
                name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                std::ptr::null_mut(),
            );
            if window.is_null() {
                anyhow::bail!("creating the window: {}", std::io::Error::last_os_error());
            }
            let mut message: MSG = std::mem::zeroed();
            while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        Ok(())
    }

    unsafe extern "system" fn session_end_proc(
        window: HWND,
        message: UINT,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            // Never the one holding a logoff or shutdown up.
            WM_QUERYENDSESSION => TRUE as LRESULT,
            WM_ENDSESSION if wparam != 0 => {
                log::info!("the session is ending; flushing and exiting");
                shut_down_before_returning();
                0
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }

    /// For a handler Windows ends the process after: the main thread is
    /// asked to keep what it keeps and exit, and given the time to.
    fn shut_down_before_returning() {
        request_shutdown();
        std::thread::sleep(EXIT_GRACE);
    }

    fn request_shutdown() {
        promise::spawn::spawn_into_main_thread(async move {
            if let Err(err) = wezterm_mux_server_impl::thinkterm_layout::flush_now() {
                log::error!("flushing ThinkTerm layouts before shutdown: {err:#}");
            }
            wezterm_mux_server_impl::thinkterm_layout::keep_pane_output();
            wezterm_mux_server_impl::share::shut_down();
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
        if unsafe { SetProcessShutdownParameters(SHUTDOWN_LEVEL, 0) } == 0 {
            log::warn!(
                "setting the shutdown level: {}",
                std::io::Error::last_os_error()
            );
        }
        if let Err(err) = watch_session_end() {
            log::warn!("{err:#}");
        }
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
