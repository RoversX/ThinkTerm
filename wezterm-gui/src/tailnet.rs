//! The app's side of an embedded tailnet (see thinkterm_tailnet): the
//! Remote Hosts page turns it on and off and lists its machines, the
//! machine's mux server holds the node -- started from here when the tailnet
//! is on and no server runs -- and the page shows how the node is doing from
//! the server's report. SSH sessions to the tailnet's machines go through the
//! server's node. Built without the `tailnet` feature the page has no
//! Tailscale group, and nothing here does anything.

use std::net::IpAddr;

// Built without the feature, nothing makes these: the group never shows.
#[cfg_attr(not(feature = "tailnet"), allow(dead_code))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Off,
    Starting,
    /// The link that approves this machine.
    NeedsLogin(String),
    Connected {
        name: String,
        address: Option<String>,
    },
    Failed(String),
    /// Another ThinkTerm process on this machine holds the node.
    HeldElsewhere,
}

/// What kind of device a machine of the tailnet is, as far as it says.
#[cfg_attr(not(feature = "tailnet"), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Device {
    Mac,
    IPhone,
    IPad,
    AppleTv,
    Android,
    Linux,
    Windows,
    /// Another system, or one that does not say.
    #[default]
    Unknown,
}

impl Device {
    /// From the system a machine reports, as Tailscale names it ("macOS",
    /// "iOS", "android", "linux", "windows"), and the model a phone or
    /// tablet reports: an iPad says iOS too, and only its model ("iPad13,4")
    /// tells it from an iPhone.
    #[cfg_attr(not(feature = "tailnet"), allow(dead_code))]
    pub fn reported(os: &str, model: &str) -> Self {
        match os.trim().to_ascii_lowercase().as_str() {
            "macos" | "darwin" => Self::Mac,
            "ios" | "ipados" if model.trim().starts_with("iPad") => Self::IPad,
            "ios" | "ipados" => Self::IPhone,
            "tvos" => Self::AppleTv,
            "android" => Self::Android,
            "linux" => Self::Linux,
            "windows" => Self::Windows,
            _ => Self::Unknown,
        }
    }

    /// What a card calls it. Names, the same in every language.
    pub fn name(self) -> Option<&'static str> {
        match self {
            Self::Mac => Some("Mac"),
            Self::IPhone => Some("iPhone"),
            Self::IPad => Some("iPad"),
            Self::AppleTv => Some("Apple TV"),
            Self::Android => Some("Android"),
            Self::Linux => Some("Linux"),
            Self::Windows => Some("Windows"),
            Self::Unknown => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Machine {
    pub name: String,
    /// An unambiguous host address: what a host saved from the machine
    /// connects to.
    pub target: String,
    /// Its name with the tailnet's domain; empty when the server did not
    /// say.
    pub fqdn: String,
    pub addresses: Vec<IpAddr>,
    pub online: Option<bool>,
    pub device: Device,
    /// Its Linux distribution ("ubuntu"), when it says.
    pub distro: String,
}

impl Machine {
    /// Whether `host`, a saved host's address, names this machine: by its
    /// name, its name with the tailnet's domain, or one of its addresses.
    pub fn answers_to(&self, host: &str) -> bool {
        let host = host.trim().trim_end_matches('.');
        if host.is_empty() {
            return false;
        }
        match host.parse::<IpAddr>() {
            Ok(ip) => self.addresses.contains(&ip),
            Err(_) => {
                self.name.eq_ignore_ascii_case(host)
                    || (!self.fqdn.is_empty()
                        && self.fqdn.trim_end_matches('.').eq_ignore_ascii_case(host))
            }
        }
    }
}

/// What the Tailscale group and its panel show.
#[derive(Clone, Debug, Default)]
pub struct View {
    pub enabled: bool,
    /// `thinkterm tailnet up` let the tailnet's machines SSH into this one.
    pub serves_ssh: bool,
    /// The other ports `thinkterm tailnet serve add` let them reach, each
    /// the same port here.
    pub served_ports: Vec<u16>,
    /// A mux server is running to hold the node; the state and the machines
    /// are its, as it last reported them.
    pub server_running: bool,
    pub control_url: String,
    pub hostname: String,
    pub has_auth_key: bool,
    /// The machine has been a node, so there is an identity to forget.
    pub has_identity: bool,
    pub state: State,
    /// Online ones first, then by name.
    pub machines: Vec<Machine>,
    /// Why the last change was not saved.
    pub error: Option<String>,
    /// Why the settings file could not be read, while it cannot: the page
    /// shows the defaults meanwhile.
    pub read_error: Option<String>,
    /// The settings have been read and, while the tailnet is on, the
    /// server's report too: until then what the page would show is a guess
    /// -- the tailnet off, or no server running.
    pub known: bool,
}

impl View {
    /// The link that approves this machine, while the control server waits
    /// for someone to.
    pub fn login_url(&self) -> Option<&str> {
        match &self.state {
            State::NeedsLogin(url) if self.enabled => Some(url),
            _ => None,
        }
    }
}

/// Online machines first, then by name, the way the group lists them.
#[cfg_attr(not(feature = "tailnet"), allow(dead_code))]
fn sort_machines(machines: &mut [Machine]) {
    machines.sort_by(|a, b| {
        let offline = |machine: &Machine| machine.online != Some(true);
        offline(a)
            .cmp(&offline(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.target.cmp(&b.target))
    });
}

pub use imp::*;

#[cfg(feature = "tailnet")]
mod imp {
    use super::{Device, Machine, State, View};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{LazyLock, Mutex};
    use std::time::{Duration, Instant};
    use thinkterm_tailnet::{Peer, Report, Settings, Status, Summary};
    use window::{Window, WindowOps};

    /// What the page paints from, until it goes (see `release`). The
    /// settings are read without their secrets.
    #[derive(Default)]
    struct Cache {
        settings: Summary,
        server: Option<Report>,
        error: Option<String>,
        refreshed_at: Option<Instant>,
        reading: bool,
        /// Bumped by every change: a read begun before one carries the
        /// settings from before it, and is not painted over them.
        generation: u64,
        /// The number of the last change saved, or tried: they are saved
        /// in the order they were made.
        saved_through: u64,
        /// The changes that could not be saved since one last was, latest
        /// last: each field whose change is among them keeps what was typed,
        /// and `error` says why the last could not. A few are kept, one for
        /// each field the page has and some to spare.
        failed: std::collections::VecDeque<u64>,
        /// Why the settings file could not be read when last read.
        read_error: Option<String>,
        /// The window of the page that read last, repainted when a change
        /// is in.
        window: Option<Window>,
        /// The settings have been read, here or at launch. They outlast a
        /// page; the server's report does not.
        settings_known: bool,
        server_known: bool,
    }

    static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

    type Change = Box<dyn FnOnce(&mut Settings) + Send>;

    /// Numbers the changes as they are made.
    static MADE: AtomicU64 = AtomicU64::new(0);

    /// The changes waiting to be saved. One saver takes them in the order
    /// they were made, so that an earlier change landing after a later one
    /// cannot undo it: a switch turned on just before Log out, say.
    static CHANGES: LazyLock<smol::channel::Sender<(u64, Change)>> = LazyLock::new(|| {
        let (changes, waiting) = smol::channel::unbounded::<(u64, Change)>();
        promise::spawn::spawn(async move {
            while let Ok((number, change)) = waiting.recv().await {
                save(number, change).await;
            }
        })
        .detach();
        changes
    });

    /// How often the page reads the settings file and the server's report
    /// again while it is on screen: both can change from outside this
    /// process.
    const POLL_INTERVAL: Duration = Duration::from_secs(2);

    /// How long after launch the app looks for a server to hold the node:
    /// by then the server local sessions run in is up, and holds it.
    const LAUNCH_WAIT: Duration = Duration::from_secs(5);

    /// How soon after starting a mux server the app may start one again: a
    /// daemonizing server takes a moment to answer on its socket.
    const SERVER_RETRY: Duration = Duration::from_secs(10);

    /// How many failed changes are remembered (see `Cache::failed`).
    const FAILURES_KEPT: usize = 8;

    /// Whether this build can join a tailnet, and so has the group.
    pub fn available() -> bool {
        thinkterm_tailnet::SUPPORTED
    }

    /// From now on: try the tailnet first for every new SSH connection, and
    /// keep a mux server running to hold the node while the tailnet is on.
    /// A host that is not one of the tailnet's machines, or one asked for
    /// while the tailnet is off, goes out as before.
    pub fn start() {
        if !available() {
            return;
        }
        thinkterm_tailnet::use_for_ssh();
        let spawned = std::thread::Builder::new()
            .name("tailnet-launch".into())
            .spawn(|| {
                seed_settings();
                std::thread::sleep(LAUNCH_WAIT);
                if thinkterm_tailnet::load_summary().is_ok_and(|settings| settings.enabled) {
                    ensure_server();
                }
            });
        if let Err(err) = spawned {
            log::warn!("tailnet: {err}");
        }
    }

    /// Reads the settings as the app starts, so that a page painting before
    /// its own read lands shows the switch as the file has it rather than
    /// off. A change made meanwhile, or a read landed, wins.
    fn seed_settings() {
        let asked_at = CACHE.lock().unwrap().generation;
        let Ok(settings) = thinkterm_tailnet::load_summary() else {
            return;
        };
        let mut cache = CACHE.lock().unwrap();
        if cache.generation == asked_at && !cache.settings_known {
            cache.settings = settings;
            cache.settings_known = true;
        }
    }

    /// Starts a mux server to hold the node unless one is running, the way
    /// local sessions start theirs; tried at most every SERVER_RETRY. One
    /// started here is not this app's to stop: it holds the node for every
    /// ThinkTerm process on the machine.
    fn ensure_server() {
        static TRIED: Mutex<Option<Instant>> = Mutex::new(None);
        {
            let mut tried = TRIED.lock().unwrap();
            if tried.is_some_and(|at| at.elapsed() < SERVER_RETRY) {
                return;
            }
            *tried = Some(Instant::now());
        }
        let spawned = std::thread::Builder::new()
            .name("tailnet-server".into())
            .spawn(|| {
                let config = config::configuration();
                let Some(unix) = crate::local_sessions::candidate(&config) else {
                    log::warn!("tailnet: no unix domain to start a mux server for");
                    return;
                };
                if wezterm_mux_server_impl::local::someone_listens(&unix.socket_path()) {
                    return;
                }
                // Nothing but the node: no shell for a workspace nothing shows.
                if let Err(err) =
                    wezterm_client::client::spawn_server(&unix, &["--no-initial-pane"])
                {
                    log::warn!("tailnet: starting the mux server: {err:#}");
                }
            });
        if let Err(err) = spawned {
            log::warn!("tailnet: {err}");
        }
    }

    pub fn view() -> View {
        let (settings, server, error, read_error, settings_known, server_known) = {
            let cache = CACHE.lock().unwrap();
            (
                cache.settings.clone(),
                cache.server.clone(),
                cache.error.clone(),
                cache.read_error.clone(),
                cache.settings_known,
                cache.server_known,
            )
        };
        let known = settings_known && (server_known || !settings.enabled);
        let server_running = server.is_some();
        let (status, peers) = server
            .map(|report| (report.status, report.peers))
            .unwrap_or_default();
        let mut machines: Vec<Machine> = peers.iter().map(machine).collect();
        super::sort_machines(&mut machines);
        View {
            enabled: settings.enabled,
            serves_ssh: settings.serve_ssh,
            served_ports: settings.serve_ports.clone(),
            server_running,
            control_url: settings.control_url.unwrap_or_default(),
            hostname: settings.hostname.unwrap_or_default(),
            has_auth_key: settings.has_auth_key,
            has_identity: settings.has_node_keys,
            state: match status {
                Status::Off => State::Off,
                Status::Starting => State::Starting,
                Status::NeedsLogin { url } => State::NeedsLogin(url),
                Status::Connected { name, address } => State::Connected {
                    name,
                    address: address.map(|address| address.to_string()),
                },
                Status::Failed { error } => State::Failed(error),
                Status::HeldElsewhere => State::HeldElsewhere,
            },
            machines,
            error,
            read_error,
            known,
        }
    }

    fn machine(peer: &Peer) -> Machine {
        Machine {
            name: peer.name.clone(),
            target: if peer.fqdn.is_empty() {
                peer.address()
                    .map(|address| address.to_string())
                    .unwrap_or_else(|| peer.name.clone())
            } else {
                peer.fqdn.clone()
            },
            fqdn: peer.fqdn.clone(),
            addresses: peer
                .ipv4
                .map(std::net::IpAddr::V4)
                .into_iter()
                .chain(peer.ipv6.map(std::net::IpAddr::V6))
                .collect(),
            online: peer.online,
            device: Device::reported(&peer.os, &peer.model),
            distro: peer.distro.clone(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn same_short_names_keep_distinct_connection_targets() {
            let first = Peer {
                name: "server-a".into(),
                fqdn: "server-a.first.example".into(),
                ipv4: Some(std::net::Ipv4Addr::new(100, 64, 0, 2)),
                ipv6: None,
                online: Some(true),
                ..Default::default()
            };
            let second = Peer {
                fqdn: "server-a.second.example".into(),
                ipv4: Some(std::net::Ipv4Addr::new(100, 64, 0, 3)),
                ..first.clone()
            };
            assert_eq!(machine(&first).name, machine(&second).name);
            assert_ne!(machine(&first).target, machine(&second).target);
            assert!(first.answers_to(&machine(&first).target));
            assert!(!second.answers_to(&machine(&first).target));
            assert!(second.answers_to(&machine(&second).target));
            let unnamed = Peer {
                fqdn: String::new(),
                ..second
            };
            assert_eq!(machine(&unnamed).target, "100.64.0.3");
        }

        #[test]
        fn a_machine_answers_to_what_its_peer_does() {
            let peer = Peer {
                name: "server-a".into(),
                fqdn: "server-a.example.ts.net".into(),
                ipv4: Some(std::net::Ipv4Addr::new(100, 64, 0, 2)),
                ipv6: Some("fd7a:115c:a1e0::2".parse().unwrap()),
                online: Some(false),
                os: "linux".into(),
                distro: "debian".into(),
                ..Default::default()
            };
            let machine = machine(&peer);
            for host in [
                "server-a",
                "SERVER-A",
                "server-a.example.ts.net.",
                "100.64.0.2",
                "fd7a:115c:a1e0:0::2",
                "server-b",
                "100.64.0.3",
            ] {
                assert_eq!(machine.answers_to(host), peer.answers_to(host), "{host}");
            }
            assert!(!machine.answers_to(""));
            assert_eq!(machine.device, Device::Linux);
            assert_eq!(machine.distro, "debian");
        }
    }

    /// Whether the page should read the files again.
    pub fn poll_is_due() -> bool {
        let cache = CACHE.lock().unwrap();
        !cache.reading
            && cache
                .refreshed_at
                .is_none_or(|at| at.elapsed() >= POLL_INTERVAL)
    }

    /// When the page should paint again to read the files: None while a
    /// read is out, which repaints when it lands.
    pub fn next_poll_at() -> Option<Instant> {
        let cache = CACHE.lock().unwrap();
        if cache.reading {
            return None;
        }
        Some(
            cache
                .refreshed_at
                .map(|at| at + POLL_INTERVAL)
                .unwrap_or_else(Instant::now),
        )
    }

    /// Reads the settings and the server's report off the UI thread, then
    /// repaints `window`. One read at a time. With the tailnet on and no
    /// server to report, one is started.
    pub fn refresh(window: Window) {
        let asked_at = {
            let mut cache = CACHE.lock().unwrap();
            cache.window = Some(window.clone());
            if std::mem::replace(&mut cache.reading, true) {
                return;
            }
            cache.generation
        };
        promise::spawn::spawn(async move {
            let (settings, server) = smol::unblock(|| {
                (
                    thinkterm_tailnet::load_summary(),
                    thinkterm_tailnet::read_live_report(),
                )
            })
            .await;
            if server.is_none() && settings.as_ref().is_ok_and(|settings| settings.enabled) {
                ensure_server();
            }
            {
                let mut cache = CACHE.lock().unwrap();
                cache.reading = false;
                if cache.generation == asked_at {
                    match settings {
                        Ok(settings) => {
                            cache.settings = settings;
                            cache.read_error = None;
                        }
                        Err(err) => {
                            log::warn!("tailnet: {err:#}");
                            cache.read_error = Some(format!("{err:#}"));
                        }
                    }
                    cache.server = server;
                    cache.refreshed_at = Some(Instant::now());
                    // Read, if not well: a file that does not parse shows
                    // as the defaults, as it did before.
                    cache.settings_known = true;
                    cache.server_known = true;
                } else {
                    // A change landed meanwhile, or the page went: read
                    // again at the next paint.
                    cache.refreshed_at = None;
                }
            }
            window.invalidate();
        })
        .detach();
    }

    /// How many Remote Hosts pages are open, each window having its own.
    static PAGES: AtomicUsize = AtomicUsize::new(0);

    /// Held by each open Remote Hosts page: the last to close lets go of
    /// what they showed (see `release`).
    pub struct Page(());

    impl Page {
        pub fn open() -> Self {
            PAGES.fetch_add(1, Ordering::SeqCst);
            Self(())
        }
    }

    impl Drop for Page {
        fn drop(&mut self) {
            if PAGES.fetch_sub(1, Ordering::SeqCst) == 1 {
                release();
            }
        }
    }

    /// Lets go of what the pages showed once the last is gone: the server's
    /// list of machines, the last error and the window. The settings stay, a
    /// few short fields, so that coming back paints them at once; they are
    /// read afresh as it does.
    fn release() {
        let mut cache = CACHE.lock().unwrap();
        cache.server = None;
        cache.server_known = false;
        cache.error = None;
        cache.failed.clear();
        cache.read_error = None;
        cache.refreshed_at = None;
        cache.window = None;
        // A read still out lands as one from before.
        cache.generation += 1;
    }

    /// Queues a change to be saved off the UI thread -- the file is locked,
    /// and waited onto the disk -- after those made before it. Returns its
    /// number (see `saved`).
    fn change(change: impl FnOnce(&mut Settings) + Send + 'static) -> u64 {
        CACHE.lock().unwrap().generation += 1;
        let number = MADE.fetch_add(1, Ordering::SeqCst) + 1;
        if CHANGES.try_send((number, Box::new(change))).is_err() {
            log::warn!("tailnet: the settings change was not saved");
        }
        number
    }

    /// Saves one change and repaints once it is in. Whatever went wrong is
    /// shown until the next change is saved. With the tailnet on, a server
    /// is started to act on it if none runs.
    async fn save(number: u64, change: Change) {
        let saved = smol::unblock(move || {
            thinkterm_tailnet::update(change).map(|settings| settings.summary())
        })
        .await;
        if saved.as_ref().is_ok_and(|settings| settings.enabled) {
            ensure_server();
        }
        let window = {
            let mut cache = CACHE.lock().unwrap();
            cache.generation += 1;
            cache.refreshed_at = None;
            cache.saved_through = number;
            match saved {
                Ok(settings) => {
                    cache.settings = settings;
                    cache.settings_known = true;
                    cache.error = None;
                    cache.failed.clear();
                    cache.read_error = None;
                }
                Err(err) => {
                    cache.error = Some(format!("{err:#}"));
                    if cache.failed.len() == FAILURES_KEPT {
                        cache.failed.pop_front();
                    }
                    cache.failed.push_back(number);
                }
            }
            cache.window.clone()
        };
        if let Some(window) = window {
            window.invalidate();
        }
    }

    /// Whether change `number` is in: None while it is still to be saved,
    /// Some(false) once it could not be, until another change is saved.
    pub fn saved(number: u64) -> Option<bool> {
        let cache = CACHE.lock().unwrap();
        (cache.saved_through >= number).then(|| !cache.failed.contains(&number))
    }

    /// The switch: on has the mux server join; off has it leave, and closes
    /// the machine to SSH from the tailnet if `thinkterm tailnet up` had
    /// opened it -- the switch never opens it -- as leaving closes the ports
    /// `thinkterm tailnet serve add` opened (see `thinkterm_tailnet::update`). Decided on the file as it is
    /// when saved, so two clicks before the first is in make two changes.
    pub fn toggle() {
        change(|settings| {
            settings.enabled = !settings.enabled;
            if !settings.enabled {
                settings.serve_ssh = false;
            }
        });
    }

    /// Has the mux server join, whatever the file said: the button that
    /// offers it shows while the tailnet is off, and a second click before
    /// the first is in changes nothing.
    pub fn turn_on() {
        change(|settings| settings.enabled = true);
    }

    /// Empty goes back to Tailscale's own (see Settings::normalize).
    /// Returns the change's number (see `saved`).
    pub fn set_control_url(text: &str) -> u64 {
        let text = text.to_string();
        change(|settings| settings.control_url = Some(text))
    }

    /// Empty goes back to the automatic name. Returns the change's number.
    pub fn set_hostname(text: &str) -> u64 {
        let text = text.to_string();
        change(|settings| settings.hostname = Some(text))
    }

    /// An empty key leaves the saved one as it is. Returns the change's
    /// number, when there is one.
    pub fn set_auth_key(text: &str) -> Option<u64> {
        if text.trim().is_empty() {
            return None;
        }
        let key = text.to_string();
        Some(change(|settings| settings.auth_key = Some(key)))
    }

    /// Leaves, and forgets the machine's identity and the auth key: the
    /// next join is a new machine.
    pub fn logout() {
        change(|settings| {
            settings.enabled = false;
            settings.serve_ssh = false;
            settings.auth_key = None;
            settings.node_keys = None;
        });
    }
}

#[cfg(not(feature = "tailnet"))]
mod imp {
    use super::View;
    use std::time::Instant;
    use window::Window;

    pub fn available() -> bool {
        false
    }
    pub fn start() {}
    pub fn view() -> View {
        View::default()
    }
    pub fn poll_is_due() -> bool {
        false
    }
    pub fn next_poll_at() -> Option<Instant> {
        None
    }
    pub fn refresh(_window: Window) {}
    pub struct Page;
    impl Page {
        pub fn open() -> Self {
            Self
        }
    }
    pub fn saved(_number: u64) -> Option<bool> {
        Some(true)
    }
    pub fn toggle() {}
    pub fn turn_on() {}
    pub fn set_control_url(_text: &str) -> u64 {
        0
    }
    pub fn set_hostname(_text: &str) -> u64 {
        0
    }
    pub fn set_auth_key(_text: &str) -> Option<u64> {
        None
    }
    pub fn logout() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_is_told_by_its_system_and_model() {
        assert_eq!(Device::reported("macOS", ""), Device::Mac);
        assert_eq!(Device::reported("iOS", "iPhone15,2"), Device::IPhone);
        assert_eq!(Device::reported("iOS", "iPad13,4"), Device::IPad);
        // An iOS device that names no model is far more often a phone.
        assert_eq!(Device::reported("iOS", ""), Device::IPhone);
        assert_eq!(Device::reported("tvOS", "AppleTV11,1"), Device::AppleTv);
        assert_eq!(Device::reported("android", "Pixel 8"), Device::Android);
        assert_eq!(Device::reported("linux", ""), Device::Linux);
        assert_eq!(Device::reported("windows", ""), Device::Windows);
        assert_eq!(Device::reported("freebsd", ""), Device::Unknown);
        // A report from before machines said: nothing to go by.
        assert_eq!(Device::reported("", ""), Device::Unknown);
        assert_eq!(Device::Unknown.name(), None);
    }

    fn machine(name: &str, online: Option<bool>) -> Machine {
        Machine {
            name: name.into(),
            target: format!("{name}.example.ts.net"),
            online,
            ..Default::default()
        }
    }

    #[test]
    fn online_machines_come_first_then_by_name() {
        let mut machines = vec![
            machine("build-box", Some(false)),
            machine("Server-b", Some(true)),
            machine("devbox", None),
            machine("server-a", Some(true)),
        ];
        sort_machines(&mut machines);
        let names: Vec<&str> = machines.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["server-a", "Server-b", "build-box", "devbox"]);
    }
}
