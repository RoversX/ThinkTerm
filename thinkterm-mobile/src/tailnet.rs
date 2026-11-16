//! The app's own node in a tailnet, so that a phone reaches the tailnet's
//! machines without the Tailscale app: the node the desktop's
//! `thinkterm-tailnet` holds for a whole machine
//! (`thinkterm_tailnet_engine::node`), held here by the app for itself.
//!
//! The shell says what the node runs on (`set`): its keys, which the shell
//! keeps where secrets go, a control server or Tailscale's own, an auth
//! key or a login in the browser. One thread starts and stops it and
//! starts it anew when it is given up, as the desktop's holder does;
//! connections to the tailnet's machines go through it (`dial`), and
//! everything else by the system's network as before. Where the library
//! does not run there is no node and nothing goes through one.

use std::time::Duration;

/// Where the node is and the machines it sees, for the shell to draw.
#[derive(uniffi::Record, Clone, Default, PartialEq)]
pub struct TailnetStatus {
    /// Grows with every change: the same number means nothing new.
    pub revision: u64,
    /// "off", "starting", "needs_login", "needs_approval" (logged in, and
    /// waiting to be let in: for an admin's approval, as a rule),
    /// "connected" or "failed".
    pub state: String,
    /// Where a person approves this device, while it needs a login.
    pub login_url: String,
    /// This device's name and address in the tailnet, once it is in.
    pub name: String,
    pub address: String,
    /// Why it is not in, when it failed -- or why what was asked of it
    /// (`tailnet_set`) could not be started on.
    pub error: String,
    pub peers: Vec<TailnetPeer>,
}

/// Another machine in the tailnet.
#[derive(uniffi::Record, Clone, Default, PartialEq)]
pub struct TailnetPeer {
    pub name: String,
    pub fqdn: String,
    /// Its addresses; empty for one it has not got.
    pub ipv4: String,
    pub ipv6: String,
    /// None while the control server has not said.
    pub online: Option<bool>,
    /// What it says it runs ("macOS", "iOS", "linux"...), its Linux
    /// distribution and a phone's model; empty where it does not say.
    pub os: String,
    pub distro: String,
    pub model: String,
}

/// What a node runs on. Blank fields mean none: Tailscale's own control
/// server, a name made from the keys, a login in the browser.
#[derive(uniffi::Record, Clone)]
pub struct TailnetConfig {
    /// The node's keys (`tailnet_new_keys`), kept by the shell.
    pub keys: String,
    pub control_url: String,
    pub hostname: String,
    pub auth_key: String,
}

/// How long a connection waits for the other machine.
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "ios", target_os = "android"))]
mod held {
    use super::{TailnetConfig, TailnetPeer, TailnetStatus, DIAL_TIMEOUT};
    use anyhow::{anyhow, Context, Result};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{mpsc, Arc, Mutex, OnceLock};
    use thinkterm_tailnet_engine::node::{self, Node, NodeConfig};
    use thinkterm_tailnet_engine::{Family, Peer, Status};

    #[derive(Default)]
    struct State {
        /// What the shell last asked for; none for no node.
        wanted: Option<NodeConfig>,
        node: Option<Arc<Node>>,
        /// Nodes given up in a row since one last got in or as far as a
        /// login: each waits longer before it is given up in turn.
        given_up: u32,
        /// Why the settings asked for could not be started on.
        error: Option<String>,
        /// The node running is to make way for a new one on the same
        /// settings (`restart`).
        restart: bool,
    }

    static STATE: Mutex<State> = Mutex::new(State {
        wanted: None,
        node: None,
        given_up: 0,
        error: None,
        restart: false,
    });
    /// When the last restart for a failed connection was asked, in seconds
    /// of `clock`: at most one a minute.
    static LAST_RESCUE: AtomicU64 = AtomicU64::new(0);
    const RESCUE_EVERY_SECS: u64 = 60;
    /// When a connection through the node last brought anything in, in
    /// seconds of `clock`. An SSH session's keepalive is answered every
    /// half minute by default, so a node whose connections are alive is
    /// heard from well within this.
    static LAST_HEARD: AtomicU64 = AtomicU64::new(0);
    const HEARD_LATELY_SECS: u64 = 90;
    static REVISION: AtomicU64 = AtomicU64::new(1);

    /// The thread that starts and stops the node: neither is done on the
    /// shell's thread (a stop waits for the node to wind down) nor on the
    /// node's own.
    fn wake() -> &'static mpsc::Sender<()> {
        static WAKE: OnceLock<mpsc::Sender<()>> = OnceLock::new();
        WAKE.get_or_init(|| {
            let (tx, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("tailnet-holder".into())
                .spawn(move || {
                    while rx.recv().is_ok() {
                        // One look for however many changes came together.
                        while rx.try_recv().is_ok() {}
                        reconcile();
                    }
                })
                .expect("spawning the tailnet's holder");
            tx
        })
    }

    fn changed() {
        REVISION.fetch_add(1, Ordering::SeqCst);
        let _ = wake().send(());
    }

    /// Makes the node what was asked for: none, the one running, or a new
    /// one in place of one on other settings or given up.
    fn reconcile() {
        let (old, start) = {
            let mut state = STATE.lock().unwrap();
            let status = state.node.as_ref().map(|node| node.status());
            if matches!(status, Some(Status::Connected { .. } | Status::NeedsLogin { .. })) {
                state.given_up = 0;
            }
            let broken = state.node.as_ref().is_some_and(|node| node.is_broken());
            let stale = match (&state.node, &state.wanted) {
                (Some(node), Some(wanted)) => {
                    broken
                        || node.config().needs_restart_for(
                            wanted,
                            &status.clone().unwrap_or_default(),
                            node.was_in(),
                        )
                }
                (Some(_), None) => true,
                (None, _) => false,
            };
            if broken {
                state.given_up += 1;
            }
            // A node asked to make way goes as one on other settings does,
            // without counting against the next one's patience. The ask is
            // taken even when the node goes for another reason: left, it
            // would stop the node started in its place.
            let restart = std::mem::take(&mut state.restart);
            let stale = stale || (restart && state.node.is_some());
            let old = if stale { state.node.take() } else { None };
            let start = match (&state.node, &state.wanted) {
                (None, Some(wanted)) => Some((wanted.clone(), state.given_up)),
                _ => None,
            };
            (old, start)
        };
        // One node on these keys at a time: the old one goes first.
        if let Some(old) = old {
            old.stop();
        }
        if let Some((config, given_up)) = start {
            let started = Node::start(config, node::broken_after(given_up), Box::new(changed));
            let mut state = STATE.lock().unwrap();
            match started {
                Ok(node) => {
                    state.node = Some(Arc::new(node));
                    state.error = None;
                }
                Err(err) => state.error = Some(format!("{err:#}")),
            }
        }
        REVISION.fetch_add(1, Ordering::SeqCst);
    }

    pub fn new_keys() -> Option<String> {
        node::new_node_keys().ok()
    }

    /// A new node in place of the one running, on the same settings: what
    /// the old one held -- its way to the control server and its relays,
    /// the paths to its peers -- belonged to a network the device may no
    /// longer be on, and a node's own watch for that does not run here
    /// (Android) or not while the app sleeps.
    pub fn restart() {
        STATE.lock().unwrap().restart = true;
        changed();
    }

    /// Seconds of the process's clock, counted from a while in: a time
    /// kept as 0 is long enough ago for each of its uses.
    fn clock() -> u64 {
        static CLOCK: OnceLock<std::time::Instant> = OnceLock::new();
        CLOCK.get_or_init(std::time::Instant::now).elapsed().as_secs() + RESCUE_EVERY_SECS.max(HEARD_LATELY_SECS)
    }

    /// A connection to a machine the node says is online did not get
    /// through: the node itself has likely gone stale, and is made anew --
    /// once a minute at most, so a machine that is merely slow to answer
    /// does not keep taking every other connection down with it. Not while
    /// another connection through the node is still heard from, as none
    /// through a stale one is: then it is that one machine that does not
    /// answer (on that port, past its firewall), and the rest are kept.
    fn rescue() {
        let now = clock();
        if now < LAST_HEARD.load(Ordering::SeqCst) + HEARD_LATELY_SECS {
            return;
        }
        let last = LAST_RESCUE.load(Ordering::SeqCst);
        if now < last + RESCUE_EVERY_SECS
            || LAST_RESCUE.compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst).is_err()
        {
            return;
        }
        restart();
    }

    pub fn set(config: Option<TailnetConfig>) -> Result<()> {
        let blank = |text: String| Some(text.trim().to_string()).filter(|text| !text.is_empty());
        let wanted = match config {
            Some(config) => {
                let wanted = NodeConfig {
                    control_url: blank(config.control_url),
                    hostname: blank(config.hostname),
                    auth_key: blank(config.auth_key),
                    node_keys: config.keys,
                    // A phone takes nothing in: there is no sshd on it.
                    forward_ssh: false,
                };
                // Found out before the node running is stopped for them.
                wanted.check().context("these settings")?;
                Some(wanted)
            }
            None => None,
        };
        {
            let mut state = STATE.lock().unwrap();
            state.wanted = wanted;
            state.error = None;
        }
        changed();
        Ok(())
    }

    fn peer(peer: Peer) -> TailnetPeer {
        TailnetPeer {
            ipv4: peer.ipv4.map(|ip| ip.to_string()).unwrap_or_default(),
            ipv6: peer.ipv6.map(|ip| ip.to_string()).unwrap_or_default(),
            name: peer.name,
            fqdn: peer.fqdn,
            online: peer.online,
            os: peer.os,
            distro: peer.distro,
            model: peer.model,
        }
    }

    pub fn status() -> TailnetStatus {
        let (node, wanted, error) = {
            let state = STATE.lock().unwrap();
            (state.node.clone(), state.wanted.is_some(), state.error.clone())
        };
        let mut said = TailnetStatus {
            revision: REVISION.load(Ordering::SeqCst),
            state: "off".into(),
            ..Default::default()
        };
        let Some(node) = node else {
            if let Some(error) = error {
                said.state = "failed".into();
                said.error = error;
            } else if wanted {
                said.state = "starting".into();
            }
            return said;
        };
        match node.status() {
            // Given up, a new node about to take its place: still on its
            // way in, why it went for a quiet line.
            Status::Failed { error } if node.is_broken() => {
                said.state = "starting".into();
                said.error = error;
            }
            status if status.awaits_approval() => said.state = "needs_approval".into(),
            Status::Off | Status::HeldElsewhere => {}
            Status::Starting => {
                said.state = "starting".into();
                // What held up its last try to get in, while it tries again
                // by itself: for a quiet line under the state, no failure.
                said.error = node.shared().problem().unwrap_or_default();
            }
            Status::NeedsLogin { url } => {
                said.state = "needs_login".into();
                said.login_url = url;
            }
            Status::Connected { name, address } => {
                said.state = "connected".into();
                said.name = name;
                said.address = address.map(|ip| ip.to_string()).unwrap_or_default();
            }
            Status::Failed { error } => {
                said.state = "failed".into();
                said.error = error;
            }
        }
        said.peers = node.peers().into_iter().map(peer).collect();
        said
    }

    /// A connection to `host` through the node, when the node has a
    /// machine of that name or address; none for a host that is not the
    /// tailnet's -- or with no node -- which goes by the system's network.
    pub async fn dial(host: &str, port: u16) -> Option<Result<tokio::io::DuplexStream>> {
        // Every host is asked about here first, the tailnet's or not. An
        // address outside the tailnet's ranges is never one of its
        // machines: it goes by the system's network without a wait.
        if host
            .trim_end_matches('.')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| !in_tailnet_ranges(ip))
        {
            return None;
        }
        // A node asked for and not there yet -- about to start, or making
        // way for a new one (`restart`) -- is waited for as one getting in
        // is: a machine of the tailnet's dialled meanwhile would go by the
        // system's network and not be found. One that could not be
        // started is not waited for.
        let until = std::time::Instant::now() + node::starting_wait(DIAL_TIMEOUT);
        // A node on its way out -- stopped for a restart while this was
        // asked of it -- answers nothing, rather than that it knows no such
        // machine or cannot reach it: the node in its place is asked again,
        // as the desktop's proxy has it, not the system's network, and the
        // failure says nothing of the new node.
        let mut again = false;
        loop {
            let node = loop {
                let (node, coming) = {
                    let state = STATE.lock().unwrap();
                    (state.node.clone(), state.wanted.is_some() && state.error.is_none())
                };
                match node {
                    Some(node) if !node.shared().is_going() => break node,
                    _ if coming && std::time::Instant::now() < until => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await
                    }
                    _ if again => return Some(Err(anyhow!("the tailnet's node stopped"))),
                    _ => return None,
                }
            };
            // The machine list is waited for only while the node is getting
            // in, and only while it is new. One that failed -- waiting for an
            // administrator's approval -- or that is still getting in long
            // after it started -- a control server it cannot reach -- may
            // stay so for long, and a host that is not the tailnet's must not
            // wait it out at every connection.
            let wait = match node.status() {
                Status::Starting => until
                    .min(node.started() + node::starting_wait(DIAL_TIMEOUT))
                    .saturating_duration_since(std::time::Instant::now()),
                _ => std::time::Duration::ZERO,
            };
            let shared = node.shared();
            let asked = {
                let (shared, host) = (Arc::clone(&shared), host.to_string());
                tokio::task::spawn_blocking(move || shared.resolve(&host, Family::Any, wait, &|| false))
            };
            let ip = match asked.await {
                Ok(Some(ip)) => ip,
                // Asked again, once, of the node in its place.
                Ok(None) if shared.is_going() && !again => {
                    again = true;
                    continue;
                }
                // A node in place that knows no such machine: not the
                // tailnet's, whichever node was asked first.
                Ok(None) if !shared.is_going() => return None,
                _ if again => return Some(Err(anyhow!("the tailnet's node stopped"))),
                _ => return None,
            };
            // Whether the machine dialled is one the node says is online, read
            // before the dial: a failure to reach it says the node is stale.
            let online = node.peers().iter().any(|peer| {
                peer.online == Some(true)
                    && (peer.ipv4.map(std::net::IpAddr::V4) == Some(ip) || peer.ipv6.map(std::net::IpAddr::V6) == Some(ip))
            });
            // The node's streams live on its runtime: the connection is made
            // and carried there, and this side holds the other end of a pipe.
            let (ours, theirs) = tokio::io::duplex(256 * 1024);
            let (answer, answered) = tokio::sync::oneshot::channel();
            let connecting = Arc::clone(&shared);
            node.spawn(async move {
                match connecting.tcp_connect(SocketAddr::new(ip, port), DIAL_TIMEOUT).await {
                    Ok(stream) => {
                        let _ = answer.send(Ok(()));
                        node::bridge(Heard(Box::pin(stream)), theirs).await;
                    }
                    Err(err) => {
                        let _ = answer.send(Err(err));
                    }
                }
            });
            match answered.await {
                Ok(Ok(())) => return Some(Ok(ours)),
                _ if shared.is_going() && !again => again = true,
                Ok(Err(err)) => {
                    // Turned away by the machine itself, it was reached: the
                    // node is not what failed. Nor is a node on its way out.
                    if online && !shared.is_going() && !node::turned_away(&err) {
                        rescue();
                    }
                    return Some(Err(err));
                }
                Err(_) => return Some(Err(anyhow!("the tailnet's node stopped"))),
            }
        }
    }

    /// A connection through the node, noting when it brings anything in
    /// (`LAST_HEARD`).
    struct Heard<S>(std::pin::Pin<Box<S>>);

    impl<S: tokio::io::AsyncRead> tokio::io::AsyncRead for Heard<S> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let before = buf.filled().len();
            let read = self.0.as_mut().poll_read(cx, buf);
            if buf.filled().len() > before {
                LAST_HEARD.store(clock(), Ordering::SeqCst);
            }
            read
        }
    }

    impl<S: tokio::io::AsyncWrite> tokio::io::AsyncWrite for Heard<S> {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.0.as_mut().poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.0.as_mut().poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            self.0.as_mut().poll_shutdown(cx)
        }
    }

    /// Whether `ip` is in the ranges a tailnet gives its machines:
    /// 100.64.0.0/10 and fd7a:115c:a1e0::/48.
    pub(super) fn in_tailnet_ranges(ip: std::net::IpAddr) -> bool {
        match ip {
            std::net::IpAddr::V4(v4) => {
                let [a, b, ..] = v4.octets();
                a == 100 && (64..128).contains(&b)
            }
            std::net::IpAddr::V6(v6) => {
                let s = v6.segments();
                s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::in_tailnet_ranges;

        #[test]
        fn only_a_tailnet_s_addresses_are_its_machines() {
            for ip in ["100.64.0.1", "100.127.255.254", "fd7a:115c:a1e0::5"] {
                assert!(in_tailnet_ranges(ip.parse().unwrap()), "{ip}");
            }
            for ip in ["100.63.255.255", "100.128.0.1", "192.0.2.10", "2001:db8::1", "fd7a:115c:a1e1::5"] {
                assert!(!in_tailnet_ranges(ip.parse().unwrap()), "{ip}");
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "ios", target_os = "android")))]
mod held {
    use super::{TailnetConfig, TailnetStatus};
    use anyhow::Result;

    pub fn new_keys() -> Option<String> {
        None
    }

    pub fn set(_config: Option<TailnetConfig>) -> Result<()> {
        anyhow::bail!("no tailnet on this system")
    }

    pub fn restart() {}

    pub fn status() -> TailnetStatus {
        TailnetStatus { state: "off".into(), ..Default::default() }
    }

    pub async fn dial(_host: &str, _port: u16) -> Option<Result<tokio::io::DuplexStream>> {
        None
    }
}

pub use held::dial;

/// Whether this build can join a tailnet by itself.
#[uniffi::export]
pub fn tailnet_supported() -> bool {
    thinkterm_tailnet_engine::SUPPORTED
}

/// The keys of a new node, for the shell to keep where secrets go and
/// hand back with `tailnet_set`: they are the device's identity in the
/// tailnet, and new ones are a new device. None where there is no node.
#[uniffi::export]
pub fn tailnet_new_keys() -> Option<String> {
    held::new_keys()
}

/// Run a node on `config`, or none. Returns at once -- `tailnet_status`
/// says how it goes -- with why not when the settings are ones no node
/// can start on (an address that is none, keys that do not read), in
/// which case whatever ran before runs on.
#[uniffi::export]
pub fn tailnet_set(config: Option<TailnetConfig>) -> String {
    match held::set(config) {
        Ok(()) => String::new(),
        Err(err) => format!("{err:#}"),
    }
}

/// Where the node is and the machines it sees.
#[uniffi::export]
pub fn tailnet_status() -> TailnetStatus {
    held::status()
}

/// The device's network changed: the node running is made anew on the
/// same settings, its connections being the old network's. Nothing with
/// no node.
#[uniffi::export]
pub fn tailnet_restart() {
    held::restart()
}
