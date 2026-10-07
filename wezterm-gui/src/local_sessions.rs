//! Local terminals kept in a background mux server.
//!
//! With the setting on, the GUI is a client of `thinkterm-mux-server` for
//! this machine too: the default unix domain is marked as the *local
//! session host* (`UnixDomain::local_session_host`), attached before the
//! first window, and made the default domain, so every local terminal is
//! spawned in the server and survives the GUI quitting, crashing or
//! updating. The in-process `local` domain stays registered as the
//! fallback and for `--domain local`.
//!
//! Everywhere the GUI decides "is this a remote host?" by finding a client
//! domain, the host is excluded: its threads are the local Space's, closing
//! its windows kills their panes, a second launch reuses this GUI. Those
//! decisions live next to the code they gate; this module only knows which
//! domain the host is.

use config::{ConfigHandle, UnixDomain};
use mux::domain::Domain;
use mux::Mux;
use std::sync::{Arc, Mutex, OnceLock};
use wezterm_client::domain::{ClientDomain, ClientDomainConfig};
use window::WindowOps as _;

/// The name of the host domain for this launch, once `install` chose one.
static HOST: OnceLock<Option<String>> = OnceLock::new();
/// Set when the host could not be attached at launch: the GUI then runs
/// its terminals in process as if the setting were off.
static FELL_BACK: Mutex<bool> = Mutex::new(false);

/// Whether this build and platform can run local sessions in a server.
/// Unix and Windows both can; what Windows lacks is the handoff that keeps
/// sessions across an update (it passes descriptors), so there the server
/// simply keeps serving and a newly launched GUI reconnects to it, and an
/// update stops it.
pub(crate) fn supported() -> bool {
    cfg!(any(unix, windows))
}

/// The setting, as it applies to this launch.
pub(crate) fn wanted() -> bool {
    supported() && crate::native_settings::local_sessions_via_mux()
}

/// The unix domain that is, or would be, the host: the configured one
/// named `unix` (the default domain every configuration has), else the
/// first one that connects to a socket rather than through a proxy.
pub(crate) fn candidate(config: &ConfigHandle) -> Option<UnixDomain> {
    candidate_in(&config.unix_domains)
}

fn candidate_in(domains: &[UnixDomain]) -> Option<UnixDomain> {
    domains
        .iter()
        .find(|dom| dom.name == "unix" && dom.proxy_command.is_none())
        .or_else(|| domains.iter().find(|dom| dom.proxy_command.is_none()))
        .cloned()
}

/// Register the host domain with the mux when the setting is on. Runs
/// before the configured client domains are registered, which skip a name
/// that already exists, so this marked copy is the one the mux keeps.
/// Returns the host's name.
pub(crate) fn install(mux: &Mux, config: &ConfigHandle) -> Option<String> {
    let name = if wanted() && config.default_domain.is_some() {
        // A configured default domain is re-applied on every config
        // reload; it wins, and the setting is left as a documented no-op.
        log::warn!(
            "local sessions: the configuration sets default_domain = {:?}, so local terminals \
             stay there and the session server is not used",
            config.default_domain.as_deref().unwrap_or_default()
        );
        None
    } else if wanted() {
        candidate(config).map(|mut unix| {
            unix.local_session_host = true;
            unix.connect_automatically = true;
            // Every pane of the host is one this GUI asked for: no default
            // shell in a workspace nothing shows.
            if let Ok(mut serve) = unix.serve_command() {
                serve.push("--no-initial-pane".into());
                unix.serve_command =
                    Some(serve.iter().map(|arg| arg.to_string_lossy().into_owned()).collect());
            }
            if mux.get_domain_by_name(&unix.name).is_none() {
                let domain: Arc<dyn Domain> =
                    Arc::new(ClientDomain::new(ClientDomainConfig::Unix(unix.clone())));
                mux.add_domain(&domain);
            }
            unix.name
        })
    } else {
        None
    };
    HOST.get_or_init(|| name.clone());
    name
}

/// The file that says the server at the default socket is the one this
/// setting runs: written when a launch attaches to it as the host, removed
/// when the server is stopped for the setting. A server the user started
/// by hand (`thinkterm-mux-server --daemonize`, `thinkterm connect unix`)
/// never gets it.
fn managed_marker() -> std::path::PathBuf {
    config::RUNTIME_DIR.join(config::runtime_file_name("local-session-host"))
}

/// This launch attached to the session server as the host.
pub(crate) fn note_host_attached() {
    if let Err(err) = std::fs::write(managed_marker(), b"") {
        log::warn!(
            "local sessions: could not record the session server as managed at {}: {err:#}",
            managed_marker().display()
        );
    }
}

/// With the setting off, the session server an earlier launch ran for it
/// is stopped: "off" means no terminals in the background, and a server
/// nothing shows would otherwise live on until the next reboot. Only the
/// server this setting started is stopped (the marker says so), and only
/// while it holds the pid file's lock, so a server the user runs for their
/// own purposes, a stale file or someone else's process is left alone.
/// Its terminals end with it, which is what turning the setting off asks
/// for. Runs once this process is the GUI, never from a launch that hands
/// its command to a GUI already running.
pub(crate) fn stop_background_server_when_off(config: &ConfigHandle) {
    if !supported() || wanted() {
        return;
    }
    let marker = managed_marker();
    if !marker.exists() {
        return;
    }
    let Some(unix) = candidate(config) else {
        return;
    };
    let socket = unix.socket_path();
    // The pid file is one per profile, the socket path is per domain: a
    // domain on a custom path may not be the server the pid file names.
    if socket != config::RUNTIME_DIR.join(config::runtime_file_name("sock")) {
        return;
    }
    if !wezterm_mux_server_impl::local::someone_listens(&socket) {
        // Gone already; nothing is managed any more.
        std::fs::remove_file(&marker).ok();
        return;
    }
    let pid_file = config.daemon_options.pid_file();
    if mux::session_server::pid_holding(&pid_file).is_none() {
        log::warn!(
            "local sessions: the setting is off but a server answers at {}; nothing holds {}, \
             so it is left running",
            socket.display(),
            pid_file.display()
        );
        return;
    }
    log::info!(
        "local sessions: the setting is off; stopping the session server at {}",
        socket.display()
    );
    match mux::session_server::stop(&pid_file, &socket, std::time::Duration::from_secs(5)) {
        Ok(_) => {
            std::fs::remove_file(&marker).ok();
        }
        Err(err) => log::warn!("local sessions: {err:#}"),
    }
}

/// The pid file and socket of the session server at the default socket.
fn server_files(config: &ConfigHandle) -> (std::path::PathBuf, std::path::PathBuf) {
    (
        config.daemon_options.pid_file(),
        config::RUNTIME_DIR.join(config::runtime_file_name("sock")),
    )
}

/// Whether a session server runs at the default socket right now.
pub(crate) fn server_running(config: &ConfigHandle) -> bool {
    let (pid_file, socket) = server_files(config);
    mux::session_server::is_running(&pid_file, &socket)
}

/// Stop the session server now, on the user's say-so: its terminals end
/// with it. Whoever started it -- this setting, a hand-run
/// `thinkterm-mux-server` -- it is the one at the default socket.
pub(crate) fn stop_server_now(config: &ConfigHandle) -> anyhow::Result<mux::session_server::StopOutcome> {
    let (pid_file, socket) = server_files(config);
    let outcome = mux::session_server::stop(&pid_file, &socket, std::time::Duration::from_secs(5))?;
    if !matches!(outcome, mux::session_server::StopOutcome::NotRunning) {
        std::fs::remove_file(managed_marker()).ok();
    }
    Ok(outcome)
}

/// "Quit and stop the session server": the stop is done once the GUI's
/// message loop has ended, so the windows close first and nothing here
/// reconnects to a server that is going away.
static STOP_AT_EXIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn stop_server_at_exit() {
    STOP_AT_EXIT.store(true, std::sync::atomic::Ordering::SeqCst);
}

pub(crate) fn finish_stop_at_exit(config: &ConfigHandle) {
    if !STOP_AT_EXIT.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    match stop_server_now(config) {
        Ok(outcome) => log::info!("session server at exit: {outcome:?}"),
        Err(err) => log::warn!("stopping the session server at exit: {err:#}"),
    }
}

/// The host's domain name for this launch, if local sessions run in it.
pub(crate) fn host_domain_name() -> Option<String> {
    if *FELL_BACK.lock().unwrap() {
        return None;
    }
    HOST.get().cloned().flatten()
}

/// The connection that serves a Space. Local Spaces keep their local
/// identity even when their terminals are hosted by the background mux.
pub(crate) fn connection_domain_for_space(space_id: &str) -> Option<String> {
    space_connection_domain(crate::workspace_threads::client_domain_for_space(space_id), host_domain_name())
}

fn space_connection_domain(remote: Option<String>, local_host: Option<String>) -> Option<String> {
    remote.or(local_host)
}

/// Whether `name` is the host domain of this launch.
pub(crate) fn is_host_domain_name(name: &str) -> bool {
    host_domain_name().as_deref() == Some(name)
}

/// Whether the domain with `id` is the host domain of this launch.
pub(crate) fn is_host_domain_id(id: mux::domain::DomainId) -> bool {
    let Some(host) = host_domain_name() else {
        return false;
    };
    Mux::get()
        .get_domain(id)
        .is_some_and(|domain| domain.domain_name() == host)
}

/// Attach the host again if the last window closing detached it (see
/// `GuiFrontEnd::forget_known_window`) and the process lived on. A window
/// opened then has to find the host's terminals mirrored before it decides
/// what to spawn, as the first window does: deciding first took a live
/// thread for a cold one and spawned its saved layout beside the terminals
/// the spawn's own attach then brought back. A failure is left to that
/// spawn, which attaches by itself.
pub(crate) async fn attach_host_if_detached() {
    let Some(name) = host_domain_name() else {
        return;
    };
    let Some(domain) = Mux::get().get_domain_by_name(&name) else {
        return;
    };
    let Some(client) = domain.downcast_ref::<ClientDomain>() else {
        return;
    };
    if client.state() == mux::domain::DomainState::Attached {
        return;
    }
    if let Err(err) = client
        .attach_with_ui(None, mux::connui::ConnectionUI::new_headless())
        .await
    {
        log::warn!("local sessions: attaching the session server for a new window: {err:#}");
    }
}

/// The host could not be attached at launch: run this launch in process.
/// The domain stays registered but detached; nothing retries it, since a
/// host attached later would find windows already classified as local.
pub(crate) fn fall_back_to_in_process(mux: &Mux, why: &anyhow::Error) {
    let Some(host) = HOST.get().cloned().flatten() else {
        return;
    };
    *FELL_BACK.lock().unwrap() = true;
    match mux.get_domain_by_name("local") {
        Some(local) => {
            mux.set_default_domain(&local);
            log::error!(
                "local sessions: could not attach the session server {host} ({why:#}); \
                 this launch runs its terminals in process"
            );
        }
        None => log::error!(
            "local sessions: could not attach the session server {host} ({why:#}) and \
             there is no in-process domain to fall back to"
        ),
    }
}

/// The host came back as a different, empty server (it crashed and was
/// started again): the terminals its mirrors show are gone. Each window
/// that showed only such mirrors rebuilds its thread from the layout store
/// into the new server -- the old mux window is moved aside first so the
/// thread's workspace counts as dead, then killed once the new one is
/// adopted. An activity guard keeps the GUI from quitting while the mux
/// is briefly empty.
pub(crate) fn on_host_replaced(domain_id: mux::domain::DomainId) {
    promise::spawn::spawn_into_main_thread(async move {
        let _activity = mux::activity::Activity::new();
        let mux = Mux::get();
        let dead_windows: Vec<mux::window::WindowId> = mux
            .iter_windows()
            .into_iter()
            .filter(|window_id| {
                mux.get_window(*window_id).is_some_and(|window| {
                    let panes: Vec<_> = window
                        .iter()
                        .flat_map(|tab| tab.iter_all_panes())
                        .collect();
                    !panes.is_empty() && panes.iter().all(|pane| pane.domain_id() == domain_id)
                })
            })
            .collect();
        log::info!(
            "local sessions: {} windows showed terminals of the replaced server; rebuilding them",
            dead_windows.len()
        );
        let rebuilt_panes: std::collections::HashSet<_> = dead_windows
            .iter()
            .filter_map(|window_id| mux.get_window(*window_id))
            .flat_map(|window| {
                window
                    .iter()
                    .flat_map(|tab| tab.iter_all_panes())
                    .map(|pane| pane.pane_id())
                    .collect::<Vec<_>>()
            })
            .collect();
        for window_id in dead_windows {
            let Some(gui) = crate::frontend::front_end().gui_window_for_mux_window(window_id)
            else {
                // Nothing shows it: gone, along with its dead mirrors.
                if let Some(window) = mux.get_window(window_id) {
                    for pane in window.iter().flat_map(|tab| tab.iter_all_panes()) {
                        if let Some(client_pane) =
                            pane.downcast_ref::<wezterm_client::pane::ClientPane>()
                        {
                            client_pane.ignore_next_kill();
                        }
                    }
                }
                mux.kill_window(window_id);
                continue;
            };
            // Moved aside quietly (a notified move would be synced into the
            // thread store and the thread would then "live" there) into a
            // workspace of its own -- only this window, not the others of
            // its workspace, which may hold live panes of other domains --
            // the thread's own workspace counts as dead; the activation
            // below materializes it again from the layout store, adopts the
            // new window into this GUI window, and kills the parked one.
            let Some(old_workspace) = mux
                .get_window(window_id)
                .map(|window| window.get_workspace().to_string())
            else {
                continue;
            };
            let dead_workspace = if old_workspace.starts_with("dead-session:") {
                old_workspace
            } else {
                let dead_workspace = format!("dead-session:{window_id}:{old_workspace}");
                mux.move_window_to_workspace_quietly(window_id, &dead_workspace);
                dead_workspace
            };
            if let Some(window) = mux.get_window(window_id) {
                for pane in window.iter().flat_map(|tab| tab.iter_all_panes()) {
                    if let Some(client_pane) =
                        pane.downcast_ref::<wezterm_client::pane::ClientPane>()
                    {
                        client_pane.ignore_next_kill();
                    }
                }
            }
            gui.window.notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                move |term_window| {
                    let thread_id = term_window.window.clone().and_then(|window| {
                        crate::workspace_threads::ensure_active_thread_for_space(
                            term_window.active_space_id(),
                        )
                        .map(|thread_id| (thread_id, window))
                    });
                    match thread_id {
                        Some((thread_id, window)) => term_window
                            .activate_workspace_thread_with_cleanup(
                                thread_id,
                                &window,
                                vec![dead_workspace],
                            ),
                        None => {
                            // Nothing to rebuild into: the parked window
                            // must not outlive this.
                            log::warn!("local sessions: no thread to rebuild for the window");
                            Mux::get().kill_window(window_id);
                        }
                    }
                },
            )));
        }
        // Windows mixing host terminals with others keep the others; only
        // the dead mirrors go (nothing is sent for them: their ids mean
        // nothing on the replacement).
        for pane in mux.iter_panes() {
            if pane.domain_id() != domain_id || rebuilt_panes.contains(&pane.pane_id()) {
                continue;
            }
            if let Some(client_pane) = pane.downcast_ref::<wezterm_client::pane::ClientPane>() {
                client_pane.ignore_next_kill();
            }
            mux.remove_pane(pane.pane_id());
        }
        smol::Timer::after(std::time::Duration::from_secs(3)).await;
    })
    .detach();
}

/// Where a terminal that is asked for "locally" goes: the host while local
/// sessions run in it, the in-process domain otherwise. For the places that
/// name `local` explicitly rather than using the default domain.
pub(crate) fn local_spawn_domain() -> config::keyassignment::SpawnTabDomain {
    config::keyassignment::SpawnTabDomain::DomainName(
        host_domain_name().unwrap_or_else(|| "local".to_string()),
    )
}

/// A layout records the domain each terminal ran in, and a terminal of the
/// host is recorded as `local` (it is one). While the setting is on, `local`
/// restores into the host; off, it restores in process. A unix domain named
/// in a layout is one the user chose and is left alone either way.
pub(crate) fn alias_recorded_domain(recorded: &str) -> Option<String> {
    alias_for(recorded, host_domain_name().as_deref())
}

fn alias_for(recorded: &str, host: Option<&str>) -> Option<String> {
    let host = host?;
    (recorded == "local").then(|| host.to_string())
}

#[cfg(test)]
mod tests {
    use super::{alias_for, candidate_in};
    use config::UnixDomain;

    /// A pid file is only believed while a process holds its lock.
    #[cfg(unix)]
    #[test]
    fn a_pid_file_counts_only_while_locked() {
        use std::io::Write as _;
        use std::os::unix::io::AsRawFd as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pid");
        let mut holder = std::fs::File::create(&path).unwrap();
        writeln!(holder, "4242").unwrap();
        assert_eq!(
            mux::session_server::pid_holding(&path),
            None,
            "nobody holds the lock"
        );
        assert_eq!(
            unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert_eq!(mux::session_server::pid_holding(&path), Some(4242));
        drop(holder);
        assert_eq!(mux::session_server::pid_holding(&path), None);
    }

    fn unix(name: &str, proxy: bool) -> UnixDomain {
        UnixDomain {
            name: name.into(),
            proxy_command: proxy.then(|| vec!["wsl".to_string()]),
            ..Default::default()
        }
    }

    #[test]
    fn the_candidate_prefers_the_default_unix_domain_over_proxies() {
        let domains = vec![unix("wsl", true), unix("other", false), unix("unix", false)];
        assert_eq!(candidate_in(&domains).map(|d| d.name), Some("unix".to_string()));
        let domains = vec![unix("wsl", true), unix("other", false)];
        assert_eq!(candidate_in(&domains).map(|d| d.name), Some("other".to_string()));
        assert!(candidate_in(&[unix("wsl", true)]).is_none());
    }

    #[test]
    fn recorded_local_terminals_follow_the_setting() {
        // On: local terminals restore into the host; a unix domain named in
        // a layout was the user's choice and remote domains are untouched.
        assert_eq!(alias_for("local", Some("unix")), Some("unix".to_string()));
        assert_eq!(alias_for("unix", Some("unix")), None);
        assert_eq!(alias_for("vm", Some("unix")), None);
        // Off: nothing is rewritten.
        assert_eq!(alias_for("local", None), None);
        assert_eq!(alias_for("unix", None), None);
    }
}

#[cfg(test)]
mod connection_state_tests {
    use super::space_connection_domain;

    #[test]
    fn local_spaces_follow_their_session_host_without_overriding_remote_spaces() {
        assert_eq!(space_connection_domain(None, Some("unix".into())), Some("unix".into()));
        assert_eq!(space_connection_domain(Some("server-a".into()), Some("unix".into())), Some("server-a".into()));
        assert_eq!(space_connection_domain(None, None), None);
    }
}
