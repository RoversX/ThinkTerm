//! A node in the tailnet, on a Tokio runtime of its own: the library needs
//! one, and what holds a node need not run on Tokio itself.

use crate::status::{Family, Peer, Status};
use anyhow::{anyhow, bail, Context, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tailscale::netstack::TcpStream;
use tailscale::{AuthState, Device};
use tokio::io::{AsyncRead, AsyncWrite};

/// How long a connection asked for while the node is still getting in
/// waits for its list of machines: at launch, restored tabs ask before the
/// node has reached the control server.
pub const STARTING_WAIT: Duration = Duration::from_secs(15);

/// How long a stopped node gets to wind down before what is left of it is
/// dropped.
const STOP_WAIT: Duration = Duration::from_secs(2);

/// The name of the node's threads.
const NODE_THREAD: &str = "tailnet";

/// What a node given up because part of it stopped says, while the one in
/// its place starts.
const STARTING_ANEW: &str = "the tailnet stopped working; starting it again";

/// How long a node may be stuck (see `is_stuck`) before it is given up and
/// started anew (see `Node::is_broken`): half a minute, doubled for each
/// node given up in a row before it, up to two, so that one left offline
/// for hours tries now and then.
pub fn broken_after(given_up: u32) -> Duration {
    Duration::from_secs(30) * 2u32.pow(given_up.min(2))
}

/// What a node starts with. No Debug: it holds the auth key and the node's
/// private keys.
#[derive(Clone, PartialEq, Eq)]
pub struct NodeConfig {
    pub control_url: Option<String>,
    pub hostname: Option<String>,
    pub auth_key: Option<String>,
    pub node_keys: String,
    /// The TCP ports it takes connections on from the tailnet, each handed
    /// to the same port on this machine: 22 to its own sshd, and whatever
    /// else its settings open. Sorted, each once: the same ports in another
    /// order are no change.
    pub forward_ports: Vec<u16>,
    /// The node, as its tailnet knows it (see [`Node::membership`]), the
    /// ports but SSH's were opened on: a node the tailnet knows otherwise --
    /// let into another tailnet on the same keys, say -- takes in none of
    /// them. SSH's is the settings' alone, as it always was.
    pub ports_opened_to: Option<String>,
}

impl NodeConfig {
    /// Whether a node running on these settings, at `status` and having got
    /// in before or not (`was_in`), has to make way for one on `new`. A new
    /// auth key alone restarts only a node it could get in: one not yet in,
    /// or one asked to log in again. It only matters for getting in, and
    /// the node that is in -- or only reconnecting -- carries connections a
    /// restart would end. Other ports taken in restart it too: the
    /// connections a port let in end with it, where a listener closed under
    /// them would leave them running, and the library closes one it drops
    /// only when its queue has room.
    pub fn needs_restart_for(&self, new: &NodeConfig, status: &Status, was_in: bool) -> bool {
        let same_node = self.control_url == new.control_url
            && self.hostname == new.hostname
            && self.node_keys == new.node_keys
            && self.forward_ports == new.forward_ports
            && self.ports_opened_to == new.ports_opened_to;
        let key_matters = !was_in || matches!(status, Status::NeedsLogin { .. });
        !same_node || (self.auth_key != new.auth_key && key_matters)
    }

    /// Whether a node can start on these settings, found out before the
    /// running one is stopped for them.
    pub fn check(&self) -> Result<()> {
        self.library_config().map(drop)
    }

    fn library_config(&self) -> Result<tailscale::Config> {
        let mut config = tailscale::Config::default();
        config.key_state =
            serde_json::from_str(&self.node_keys).context("reading the node's keys")?;
        config.client_name = Some("thinkterm".to_string());
        config.requested_hostname = Some(
            self.hostname
                .clone()
                .unwrap_or_else(|| automatic_name(&config.key_state)),
        );
        if let Some(url) = &self.control_url {
            config.control_server_url = crate::control_url(url)?;
        }
        Ok(config)
    }
}

pub struct Node {
    config: NodeConfig,
    /// When it was started.
    started: Instant,
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
    shared: Arc<Shared>,
    /// What its holder has done with it stopped (see `Node::at_stop`).
    at_stop: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

pub struct Shared {
    state: Mutex<(Status, Vec<Peer>)>,
    device: Mutex<Option<Arc<Device>>>,
    /// The tailnet's own domain, as this machine's full name has it.
    domain: Mutex<Option<String>>,
    /// This node as its tailnet knows it (see `Node::membership`), once the
    /// node has its netmap.
    membership: Mutex<Option<String>>,
    /// Where the node listens for the tailnet, SSH's port included: each
    /// listener once it has bound, until it ends (see `Listening`).
    listening: Mutex<std::collections::BTreeSet<SocketAddr>>,
    /// Set once the node is stopped: what waits on it gives up.
    stopped: AtomicBool,
    /// Set once the node is given up, to be started anew (see
    /// `Node::is_broken`).
    broken: AtomicBool,
    /// Set once the node has got in.
    was_in: AtomicBool,
    /// What held up the node's last try to get in, while the library tries
    /// again by itself (its sign-in retry): the status says it is still
    /// getting in, this says why it is taking a while.
    problem: Mutex<Option<String>>,
    changed: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    /// Gives the node up, to be started anew in its place, and says so.
    fn give_up(&self, status: Status) {
        self.broken.store(true, Ordering::SeqCst);
        let unchanged = self.status() == status;
        self.set_status(status);
        if unchanged {
            (self.changed)();
        }
    }

    /// Gives the node up, as `give_up`, unless it got in -- or as far as a
    /// login -- since it was last looked at: looked at again under the
    /// lock its status changes under, so it cannot get in between.
    fn give_up_if_stuck(&self) -> bool {
        {
            let state = self.state.lock().unwrap();
            if !is_stuck(&state.0) {
                return false;
            }
            self.broken.store(true, Ordering::SeqCst);
        }
        (self.changed)();
        true
    }

    fn set_status(&self, status: Status) {
        {
            let mut state = self.state.lock().unwrap();
            if state.0 == status {
                return;
            }
            if matches!(status, Status::Connected { .. }) {
                self.was_in.store(true, Ordering::SeqCst);
            }
            state.0 = status;
        }
        (self.changed)();
    }

    fn set_peers(&self, peers: Vec<Peer>) {
        {
            let mut state = self.state.lock().unwrap();
            if state.1 == peers {
                return;
            }
            state.1 = peers;
        }
        (self.changed)();
    }

    fn status(&self) -> Status {
        self.state.lock().unwrap().0.clone()
    }

    /// What held up the node's last try to get in, while it tries again;
    /// none once it got past it.
    pub fn problem(&self) -> Option<String> {
        self.problem.lock().unwrap().clone()
    }

    /// A question to the library that failed, as it failed. Where a part of
    /// the library is gone (`library_gone`) the node is given up at once,
    /// once it was in: left to the watchdog's patience it would carry
    /// nothing for half a minute or more -- a part that stopped while the
    /// app was away, say. One that never got in is left to the watchdog, so
    /// that a library that cannot start is not made anew again and again.
    fn library_failed(&self, err: tailscale::Error) -> anyhow::Error {
        if library_gone(&err) && self.was_in.load(Ordering::SeqCst) {
            self.give_up(Status::Failed {
                error: STARTING_ANEW.into(),
            });
        }
        anyhow!(err)
    }

    fn set_problem(&self, problem: Option<String>) {
        {
            let mut held = self.problem.lock().unwrap();
            if *held == problem {
                return;
            }
            *held = problem;
        }
        (self.changed)();
    }

    /// Whether the node is on its way out: stopped, or given up to be
    /// started anew. What is asked of it is better asked of the node in its
    /// place.
    pub fn is_going(&self) -> bool {
        self.stopped.load(Ordering::SeqCst) || self.broken.load(Ordering::SeqCst)
    }

    fn peers(&self) -> Vec<Peer> {
        self.state.lock().unwrap().1.clone()
    }

    /// The selected tailnet's address of `family` for `host`, or None when
    /// its machine list cannot resolve it. Waits up to `wait` while joining.
    pub fn resolve(
        &self,
        host: &str,
        family: Family,
        wait: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Option<IpAddr> {
        let given_up = || cancelled() || self.stopped.load(Ordering::SeqCst);
        resolve_with(
            host,
            family,
            wait,
            &given_up,
            || self.status(),
            || self.peers(),
            || self.domain.lock().unwrap().clone(),
        )
    }

    /// From inside the node's runtime: a stream to `to` through the
    /// tailnet.
    pub async fn tcp_connect(&self, to: SocketAddr, timeout: Duration) -> Result<TcpStream> {
        let device = self
            .device
            .lock()
            .unwrap()
            .clone()
            .context("the tailnet is not connected yet")?;
        match tokio::time::timeout(timeout, device.tcp_connect(to)).await {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(err)) => {
                // A part of the library gone -- its netstack, an actor of its
                // runtime -- the node connects nowhere again while saying it
                // is in: nothing else would notice soon.
                if library_gone(&err) {
                    self.give_up(Status::Failed {
                        error: STARTING_ANEW.into(),
                    });
                }
                Err(anyhow!(err))
            }
            Err(_) => bail!("no answer within {timeout:?}"),
        }
    }
}

/// How long a connection waits for a node that is still getting in, out of
/// the time it has.
pub fn starting_wait(timeout: Duration) -> Duration {
    timeout.min(STARTING_WAIT)
}

/// Whether the library failed because a part of it is gone -- its netstack,
/// an actor of its runtime -- which nothing brings back: the node connects
/// nowhere again, whatever its status still says.
fn library_gone(err: &tailscale::Error) -> bool {
    use tailscale::InternalErrorKind::{Actor, InternalChannelClosed};
    matches!(err, tailscale::Error::Internal(Actor | InternalChannelClosed))
}

/// Whether `Shared::tcp_connect` failed because the machine itself turned
/// the connection away (nothing listens on that port, say): it was
/// reached, so the way there works.
pub fn turned_away(err: &anyhow::Error) -> bool {
    matches!(err.downcast_ref::<tailscale::Error>(), Some(tailscale::Error::ConnectionReset))
}

/// Shared::resolve, with what it asks passed in.
fn resolve_with(
    host: &str,
    family: Family,
    wait: Duration,
    cancelled: &dyn Fn() -> bool,
    status: impl Fn() -> Status,
    peers: impl Fn() -> Vec<Peer>,
    domain: impl Fn() -> Option<String>,
) -> Option<IpAddr> {
    let host = host.trim_end_matches('.');
    let literal = host.parse::<IpAddr>().ok();
    if literal.is_some_and(|ip| !family.allows(ip)) {
        return None;
    }
    let deadline = Instant::now().checked_add(wait);
    loop {
        if cancelled() {
            return None;
        }
        let named: Vec<Peer> = peers()
            .into_iter()
            .filter(|peer| peer.answers_to(host))
            .collect();
        if !named.is_empty() {
            let peer = one_named(named, domain().as_deref())?;
            return literal.or_else(|| peer.address_in(family));
        }
        // The caller selected this tailnet. Its machine list alone resolves
        // the name, once the node has joined; no system DNS is asked.
        if !matches!(status(), Status::Starting | Status::Failed { .. })
            || deadline.is_some_and(|by| Instant::now() >= by)
        {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The machine a name stands for when several answer to it: the tailnet's
/// own before one shared into it from another tailnet. None when only
/// others share it -- which one is meant cannot be told, and their full
/// names can say.
fn one_named(mut named: Vec<Peer>, domain: Option<&str>) -> Option<Peer> {
    let own = domain.and_then(|domain| named.iter().position(|peer| peer.in_domain(domain)));
    if let Some(at) = own {
        return Some(named.swap_remove(at));
    }
    (named.len() == 1).then(|| named.swap_remove(0))
}

/// The keys of a new node, as [`NodeConfig::node_keys`] keeps them.
pub fn new_node_keys() -> Result<String> {
    Ok(serde_json::to_string(
        &tailscale::keys::PersistState::default(),
    )?)
}

/// The name a node asks the tailnet for when none is set: "thinkterm-" and
/// four hex digits of the machine's public key. Not the system's host
/// name, which often carries its owner's; and not none, which Headscale
/// names "node". It stays while the machine keeps its keys and changes
/// with them at logout, so the new node does not take the old one's name
/// while the tailnet still lists it.
pub fn automatic_name(keys: &tailscale::keys::PersistState) -> String {
    let public = keys.machine_key.import().public;
    let key = public.to_x25519_dalek().to_bytes();
    format!("thinkterm-{:02x}{:02x}", key[0], key[1])
}

/// Whether the control server at `url` sends its key within 15 seconds.
async fn control_server_answers(url: &url::Url) -> Result<()> {
    let wait = Duration::from_secs(15);
    match tokio::time::timeout(wait, ts_control::client::fetch_control_key(url)).await {
        Ok(Ok(_key)) => Ok(()),
        Ok(Err(err)) => Err(anyhow!(err)),
        Err(_) => Err(anyhow!("no answer within {wait:?}")),
    }
}

impl Node {
    /// Starts joining. Returns at once; the status says how it goes, and
    /// `changed` is called (on the node's own threads) whenever the status
    /// or the list of peers changes, or the node is given up (see
    /// `Node::is_broken`).
    pub fn start(
        config: NodeConfig,
        broken_after: Duration,
        changed: Box<dyn Fn() + Send + Sync>,
    ) -> Result<Self> {
        let library_config = config.library_config()?;
        let control = library_config.control_server_url.clone();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name(NODE_THREAD)
            .enable_all()
            .build()
            .context("starting the tailnet's runtime")?;
        let shared = Arc::new(Shared {
            state: Mutex::new((Status::Starting, vec![])),
            device: Mutex::new(None),
            domain: Mutex::new(None),
            membership: Mutex::new(None),
            listening: Mutex::new(std::collections::BTreeSet::new()),
            stopped: AtomicBool::new(false),
            broken: AtomicBool::new(false),
            was_in: AtomicBool::new(false),
            problem: Mutex::new(None),
            changed,
        });
        watched(
            runtime.handle(),
            &shared,
            run(
                Arc::clone(&shared),
                library_config,
                config.auth_key.clone(),
                config.forward_ports.clone(),
                config.ports_opened_to.clone(),
            ),
        );
        watched(
            runtime.handle(),
            &shared,
            watchdog(Arc::clone(&shared), broken_after, move || {
                let control = control.clone();
                async move { control_server_answers(&control).await.is_ok() }
            }),
        );
        Ok(Self {
            config,
            started: Instant::now(),
            runtime: Mutex::new(Some(runtime)),
            shared,
            at_stop: Mutex::new(Vec::new()),
        })
    }

    /// The half of the node what runs beside it holds: where it is, the
    /// machines it sees and the way through it.
    pub fn shared(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    /// Runs `task` beside the node on its runtime, for as long as the
    /// node runs and watched as its own loops are: one that panics gives
    /// the node up. A holder's way of answering for the node -- to the
    /// machine's other processes, say. Nothing runs on a node stopped.
    pub fn spawn(&self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        if let Some(runtime) = self.runtime.lock().unwrap().as_ref() {
            watched(runtime.handle(), &self.shared, task);
        }
    }

    /// Has `done` called once the node has stopped: for what its holder
    /// set up beside it and has to take down after it.
    pub fn at_stop(&self, done: impl FnOnce() + Send + 'static) {
        self.at_stop.lock().unwrap().push(Box::new(done));
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    /// When the node was started.
    pub fn started(&self) -> Instant {
        self.started
    }

    pub fn status(&self) -> Status {
        self.shared.status()
    }

    pub fn peers(&self) -> Vec<Peer> {
        self.shared.peers()
    }

    /// This node as its tailnet knows it -- the tailnet's name and the id
    /// its control server gave the node -- once it has its netmap: what the
    /// ports but SSH's are opened to (see [`NodeConfig::ports_opened_to`]).
    pub fn membership(&self) -> Option<String> {
        self.shared.membership.lock().unwrap().clone()
    }

    /// The ports the node listens on for the tailnet, SSH's included: on
    /// one of its addresses at least, and only once the listener has bound.
    pub fn forwarding(&self) -> Vec<u16> {
        let mut ports: Vec<u16> = self
            .shared
            .listening
            .lock()
            .unwrap()
            .iter()
            .map(SocketAddr::port)
            .collect();
        ports.sort_unstable();
        ports.dedup();
        ports
    }

    /// Leaves the tailnet: every connection through the node ends with it,
    /// and one still waiting on it gives up. Waits a moment for the node to
    /// wind down, so that one started next on the same keys does not meet
    /// it at the control server. Not from inside a Tokio runtime.
    pub fn stop(&self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        let device = self.shared.device.lock().unwrap().take();
        // Taken out, not held while it winds down: `spawn` asks for it
        // from other runtimes' threads, and finds none at once.
        let runtime = self.runtime.lock().unwrap().take();
        if let Some(runtime) = runtime {
            // The library's own way down first, for as long as it takes
            // within half the wait; what is left of it goes with the
            // runtime.
            if let Some(device) = device {
                runtime.block_on(device.stop(STOP_WAIT / 2));
            }
            runtime.shutdown_timeout(STOP_WAIT);
        }
        for done in std::mem::take(&mut *self.at_stop.lock().unwrap()) {
            done();
        }
    }

    /// Whether the node was given up, to be started anew: it was stuck for
    /// its `broken_after` (see `watchdog`), or one of its loops panicked.
    /// The library tries the control server only a couple of times before it
    /// stops trying, and a part of it whose restarts ran out stays stopped:
    /// neither comes back by itself.
    pub fn is_broken(&self) -> bool {
        self.shared.broken.load(Ordering::SeqCst)
    }

    /// Whether the node has got in: one that has reconnects by itself.
    pub fn was_in(&self) -> bool {
        self.shared.was_in.load(Ordering::SeqCst)
    }
}

async fn run(
    shared: Arc<Shared>,
    config: tailscale::Config,
    auth_key: Option<String>,
    forward_ports: Vec<u16>,
    ports_opened_to: Option<String>,
) {
    let mut wait = Duration::from_secs(5);
    let device = loop {
        shared.set_status(Status::Starting);
        match Device::new(&config, auth_key.clone()).await {
            Ok(device) => break Arc::new(device),
            Err(err) => shared.set_status(Status::Failed {
                error: err.to_string(),
            }),
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(60));
    };
    *shared.device.lock().unwrap() = Some(Arc::clone(&device));

    let mut listeners: Vec<(SocketAddr, tokio::task::JoinHandle<()>)> = Vec::new();
    let mut told_elsewhere = false;
    let mut asking_authorization = None;
    let mut asking_netmap = None;
    loop {
        let snapshot = authorized_snapshot(
            answer(&mut asking_authorization, || {
                let device = Arc::clone(&device);
                let shared = Arc::clone(&shared);
                async move {
                    // Turned away by the control server: why, rather than
                    // a node that seems never to get anywhere. A device not
                    // let in, in the words whoever shows the node tells it
                    // by; any other failure the library asks again by itself
                    // (its sign-in retry), the node still on its way in and
                    // held up by it meanwhile.
                    match device
                        .registration_error()
                        .await
                        .map_err(|err| shared.library_failed(err))?
                    {
                        Some(reason) if reason == tailscale::REGISTRATION_NOT_LET_IN => {
                            shared.set_problem(None);
                            bail!(crate::NOT_LET_IN);
                        }
                        Some(reason) => {
                            shared.set_problem(Some(reason));
                            return Ok(None);
                        }
                        None => shared.set_problem(None),
                    }
                    device
                        .current_auth_state()
                        .await
                        .map_err(|err| shared.library_failed(err))
                }
            }),
            answer(&mut asking_netmap, || {
                let device = Arc::clone(&device);
                let shared = Arc::clone(&shared);
                async move {
                    // Both come with the first netmap; until then the node is
                    // still getting in, the machines included.
                    let Some(this) = device
                        .current_self_node()
                        .await
                        .map_err(|err| shared.library_failed(err))?
                    else {
                        return Ok(None);
                    };
                    let Some(nodes) = device.peers().await.map_err(|err| shared.library_failed(err))? else {
                        return Ok(None);
                    };
                    Ok(Some((this, nodes)))
                }
            }),
            Duration::from_secs(5),
        )
        .await;
        // Given up meanwhile, or stopped: the node in its place carries on,
        // and this one's status stays as it was left -- why it went.
        if shared.is_going() {
            return;
        }
        match snapshot {
            Ok((this, nodes)) => {
                let addresses: Vec<IpAddr> = [
                    IpAddr::V4(this.tailnet_address.ipv4.addr()),
                    IpAddr::V6(this.tailnet_address.ipv6.addr()),
                ]
                .into_iter()
                .filter(|ip| !ip.is_unspecified())
                .collect();
                let membership = membership_of(&this);
                let ports = taken_in(&forward_ports, ports_opened_to.as_deref(), &membership);
                if ports.len() < forward_ports.len() && !told_elsewhere {
                    told_elsewhere = true;
                    log::warn!(
                        "tailnet: ports were opened to this machine as another tailnet knew it; \
                         `thinkterm tailnet serve add` opens them to this one"
                    );
                }
                // At most one listener per port taken in and current node
                // address. A fresh authorization may give the node new
                // addresses, or make it another node.
                listeners.retain(|(on, task)| {
                    let keep = addresses.contains(&on.ip())
                        && ports.contains(&on.port())
                        && !task.is_finished();
                    if !keep {
                        task.abort();
                    }
                    keep
                });
                for &address in &addresses {
                    for &port in &ports {
                        let on = SocketAddr::new(address, port);
                        if !listeners.iter().any(|(listening, _)| *listening == on) {
                            listeners.push((
                                on,
                                tokio::spawn(forward_to_this_machine(
                                    Arc::clone(&device),
                                    Arc::clone(&shared),
                                    on,
                                )),
                            ));
                        }
                    }
                }
                *shared.domain.lock().unwrap() = this.tailnet.clone();
                *shared.membership.lock().unwrap() = Some(membership);
                let mut peers: Vec<Peer> = nodes.iter().map(peer).collect();
                peers.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.fqdn.cmp(&b.fqdn)));
                shared.set_peers(peers);
                shared.set_status(Status::Connected {
                    name: this.hostname,
                    address: addresses.first().copied(),
                });
            }
            Err(status) => {
                if matches!(status, Status::NeedsLogin { .. }) {
                    shared.set_peers(vec![]);
                }
                shared.set_status(status);
            }
        }
        let wait = match shared.status() {
            Status::Starting => Duration::from_millis(250),
            Status::NeedsLogin { .. } | Status::Failed { .. } => Duration::from_secs(2),
            _ => Duration::from_secs(5),
        };
        tokio::time::sleep(wait).await;
    }
}

/// Check authorization on every refresh. A control reconnect can require
/// a new login even after this node has been connected. Reading the node
/// can lack a netmap. Use current-state reads so an offline control server
/// retains no abandoned requests, and bound the whole read as well.
async fn authorized_snapshot<T>(
    authorization: impl std::future::Future<Output = Result<Option<AuthState>>>,
    snapshot: impl std::future::Future<Output = Result<Option<T>>>,
    wait: Duration,
) -> std::result::Result<T, Status> {
    let failed = |err: anyhow::Error| Status::Failed {
        error: err.to_string(),
    };
    let read = async {
        match authorization
            .await
            .map_err(&failed)?
            .ok_or(Status::Starting)?
        {
            AuthState::Authorized => snapshot.await.map_err(&failed)?.ok_or(Status::Starting),
            AuthState::NotAuthorized(url) => Err(Status::NeedsLogin {
                url: url.to_string(),
            }),
        }
    };
    tokio::time::timeout(wait, read)
        .await
        .unwrap_or(Err(Status::Starting))
}

/// The answer to a question for the library, asked in a task kept in
/// `asking` until it is answered. A question stays in the library's
/// mailboxes after we stop waiting for it, and its control client handles
/// some messages by waiting on the network: one still unanswered is waited
/// for again rather than asked again, so a stuck library is not flooded.
async fn answer<T, F>(
    asking: &mut Option<tokio::task::JoinHandle<Result<T>>>,
    ask: impl FnOnce() -> F,
) -> Result<T>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T>> + Send + 'static,
{
    let task = asking.get_or_insert_with(|| tokio::spawn(ask()));
    let answered = task.await;
    *asking = None;
    answered?
}

/// How long a node has gone without getting in, or as far as a login.
#[derive(Default)]
struct Stuck(Option<Instant>);

impl Stuck {
    /// Counts a look at the node, `stuck` or not: whether every look has
    /// found it stuck for at least `long` by `now`.
    fn for_at_least(&mut self, stuck: bool, long: Duration, now: Instant) -> bool {
        if !stuck {
            self.0 = None;
            return false;
        }
        now.saturating_duration_since(*self.0.get_or_insert(now)) >= long
    }
}

/// Gives the node up once it has been stuck (see `is_stuck`) for
/// `broken_after`, wherever it is stuck: building the library, waiting on a
/// control server the library stopped trying, on a control connection that
/// went with the network it was made on, or with part of the library
/// stopped. One that was in goes only once `control_answers`: until then
/// one started in its place could not get in either, while this one may
/// still carry connections to the machines it knew.
async fn watchdog<F>(shared: Arc<Shared>, broken_after: Duration, control_answers: impl Fn() -> F)
where
    F: std::future::Future<Output = bool>,
{
    let mut stuck = Stuck::default();
    let mut was_in = false;
    loop {
        tokio::time::sleep(broken_after / 30).await;
        let status = shared.status();
        was_in |= !is_stuck(&status);
        if !stuck.for_at_least(is_stuck(&status), broken_after, Instant::now()) {
            continue;
        }
        // Looked at again after another while, unless the node got in
        // while its control server was asked.
        if was_in && !(control_answers().await && is_stuck(&shared.status())) {
            stuck = Stuck::default();
            continue;
        }
        let status = shared.status();
        // One that got in at the last moment is watched on, not given up.
        if !shared.give_up_if_stuck() {
            stuck = Stuck::default();
            continue;
        }
        log::warn!("tailnet: stuck for {broken_after:?} ({status:?}); starting the node anew");
        return;
    }
}

/// Whether a node is stuck at `status`: anywhere short of in, or as far as
/// a login. Connections through one that is not in time out: the library
/// asks its control part for every one. Not one waiting for an admin's
/// approval: the library asks control again by itself at a steady pace (its
/// sign-in retry) and gets in once approved, where a node made anew would
/// only ask the same, later.
fn is_stuck(status: &Status) -> bool {
    !matches!(status, Status::Connected { .. } | Status::NeedsLogin { .. }) && !status.awaits_approval()
}

/// Runs one of the node's own loops. One that panics leaves nothing to
/// notice that it stopped, so the node is given up and started anew.
fn watched(
    runtime: &tokio::runtime::Handle,
    shared: &Arc<Shared>,
    task: impl std::future::Future<Output = ()> + Send + 'static,
) {
    let task = runtime.spawn(task);
    let shared = Arc::clone(shared);
    runtime.spawn(async move {
        if task.await.is_err_and(|err| err.is_panic()) {
            shared.give_up(Status::Failed {
                error: STARTING_ANEW.into(),
            });
        }
    });
}

fn peer(node: &tailscale::NodeInfo) -> Peer {
    let ipv4: Ipv4Addr = node.tailnet_address.ipv4.addr();
    let ipv6: Ipv6Addr = node.tailnet_address.ipv6.addr();
    Peer {
        name: node.hostname.clone(),
        fqdn: node.fqdn(false),
        ipv4: (!ipv4.is_unspecified()).then_some(ipv4),
        ipv6: (!ipv6.is_unspecified()).then_some(ipv6),
        online: match node.status {
            ts_control::NodeStatus::Online => Some(true),
            ts_control::NodeStatus::Offline(_) => Some(false),
            ts_control::NodeStatus::Unknown => None,
        },
        os: node.device.os.clone(),
        distro: node.device.distro.clone(),
        model: node.device.device_model.clone(),
    }
}

/// SSH's port, handed to this machine's own sshd.
const SSH_PORT: u16 = 22;

/// This node as its tailnet knows it: the tailnet's name and the id its
/// control server gave the node. Let into another tailnet, even on the same
/// keys and control server, it is another.
fn membership_of(this: &tailscale::NodeInfo) -> String {
    format!(
        "{}/{}",
        this.tailnet.as_deref().unwrap_or_default(),
        this.stable_id.0
    )
}

/// Of the ports a node is to take in, the ones it does as the tailnet knows
/// it now (`membership`): SSH's, which its settings alone open, and the
/// others only if they were opened to it as it is.
fn taken_in(forward_ports: &[u16], opened_to: Option<&str>, membership: &str) -> Vec<u16> {
    forward_ports
        .iter()
        .copied()
        .filter(|&port| port == SSH_PORT || opened_to == Some(membership))
        .collect()
}

/// A port on this machine's tailnet address, `on`, handed to the same port
/// on the machine itself: 22 to its own sshd, another to whatever listens
/// there. That is all the node lets in, and only the ports its settings
/// open (`thinkterm tailnet up --serve-ssh`, `thinkterm tailnet serve add`).
/// What listens sees a loopback client, so its rules for local connections
/// apply.
async fn forward_to_this_machine(device: Arc<Device>, shared: Arc<Shared>, on: SocketAddr) {
    let listener = match device.tcp_listen(on).await {
        Ok(listener) => listener,
        Err(err) => {
            log::error!("tailnet: listening on {on}: {err}");
            return;
        }
    };
    let _listening = Listening::new(Arc::clone(&shared), on);
    loop {
        match listener.accept().await {
            Ok(stream) => {
                tokio::spawn(hand_to_this_machine(stream, on.port()));
            }
            // The netstack gone, nothing comes in again: the node is given
            // up rather than asked every second.
            Err(tailscale::netstack::Error::Internal(
                tailscale::netstack::InternalErrorKind::InternalChannelClosed,
            )) => {
                shared.give_up(Status::Failed {
                    error: STARTING_ANEW.into(),
                });
                return;
            }
            Err(err) => {
                log::warn!("tailnet: accepting a connection on {on}: {err}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// A listener that has bound, in `Shared::listening` for as long as it runs:
/// what the node reports it takes in. Its going, aborted among others, is
/// told as its coming is.
struct Listening {
    shared: Arc<Shared>,
    on: SocketAddr,
}

impl Listening {
    fn new(shared: Arc<Shared>, on: SocketAddr) -> Self {
        shared.listening.lock().unwrap().insert(on);
        (shared.changed)();
        Self { shared, on }
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.shared.listening.lock().unwrap().remove(&self.on);
        (self.shared.changed)();
    }
}

/// How long the local side of a connection whose tailnet side has ended
/// may go without sending before it is ended too: a service that answers
/// does so sooner, one that keeps the connection after the end of the
/// request would keep it for as long as the node runs.
const ANSWER_IDLE: Duration = Duration::from_secs(60);

/// Carries a connection that came in from the tailnet on `port` to the same
/// port here, over loopback: IPv4's first, then IPv6's, so that a server
/// listening on either one alone takes it. SSH's goes as it always went
/// (see `bridge`), the others as `bridge_to_local` carries them.
async fn hand_to_this_machine(stream: impl AsyncRead + AsyncWrite, port: u16) {
    let here = [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ];
    match tokio::net::TcpStream::connect(&here[..]).await {
        Ok(local) if port == SSH_PORT => bridge(stream, local).await,
        Ok(local) => bridge_to_local(stream, local, ANSWER_IDLE).await,
        Err(err) if port == SSH_PORT => {
            log::warn!("tailnet: an SSH connection came in, but no sshd answers on this machine: {err}")
        }
        Err(err) => log::warn!(
            "tailnet: a connection came in on port {port}, but nothing on this machine answers on it: {err}"
        ),
    }
}

/// Copies a connection from the tailnet to one on this machine both ways.
/// Once the tailnet's side has sent all it will, the local side is told so
/// -- a TCP connection here can half-close -- and its answer still goes
/// back: what a client that sends its request and then its end waits for.
/// The local side ending, either failing, or the local side sending nothing
/// for `idle` after the tailnet's has ended, ends both: the tailnet's
/// streams cannot half-close (see `bridge`).
async fn bridge_to_local(
    remote: impl AsyncRead + AsyncWrite,
    local: tokio::net::TcpStream,
    idle: Duration,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut remote_read, mut remote_write) = tokio::io::split(remote);
    let (mut local_read, mut local_write) = local.into_split();
    let (ended, ends) = (AtomicBool::new(false), tokio::sync::Notify::new());
    let (ended, ends) = (&ended, &ends);
    let to_local = async {
        if tokio::io::copy(&mut remote_read, &mut local_write).await.is_ok()
            && local_write.shutdown().await.is_ok()
        {
            ended.store(true, Ordering::SeqCst);
            ends.notify_one();
            // The answer is the other direction's to carry.
            std::future::pending::<()>().await;
        }
    };
    // Once the tailnet's side has ended: `idle` from the last the local
    // side sent.
    let quiet_after_the_end = move || async move {
        if !ended.load(Ordering::SeqCst) {
            ends.notified().await;
        }
        tokio::time::sleep(idle).await;
    };
    let to_remote = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let read = tokio::select! {
                read = local_read.read(&mut buf) => read,
                _ = quiet_after_the_end() => return,
            };
            match read {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if remote_write.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            }
        }
    };
    tokio::select! {
        _ = to_remote => {}
        _ = to_local => {}
    }
}

/// Copies both ways until either side ends, then closes both. The tailnet's
/// streams cannot half-close, so waiting on the other direction after one
/// has ended could wait forever.
pub async fn bridge(a: impl AsyncRead + AsyncWrite, b: impl AsyncRead + AsyncWrite) {
    let (mut a_read, mut a_write) = tokio::io::split(a);
    let (mut b_read, mut b_write) = tokio::io::split(b);
    tokio::select! {
        _ = tokio::io::copy(&mut a_read, &mut b_write) => {}
        _ = tokio::io::copy(&mut b_read, &mut a_write) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> NodeConfig {
        NodeConfig {
            control_url: Some("https://headscale.example.com".into()),
            hostname: Some("server-a".into()),
            auth_key: Some("tskey-old".into()),
            node_keys: new_node_keys().unwrap(),
            forward_ports: vec![22],
            ports_opened_to: None,
        }
    }

    #[test]
    fn a_new_auth_key_alone_restarts_only_a_node_not_yet_in() {
        let running = config();
        let connected = Status::Connected {
            name: "server-a".into(),
            address: None,
        };
        let new_key = NodeConfig {
            auth_key: Some("tskey-new".into()),
            ..running.clone()
        };
        let login = Status::NeedsLogin {
            url: "https://login.example.com/a".into(),
        };
        assert!(!running.needs_restart_for(&running.clone(), &connected, true));
        assert!(!running.needs_restart_for(&new_key, &connected, true));
        // Only reconnecting: the key would not be asked for.
        assert!(!running.needs_restart_for(&new_key, &Status::Starting, true));
        // Not in yet, or to log in again: the key may get it in.
        assert!(running.needs_restart_for(&new_key, &Status::Starting, false));
        assert!(running.needs_restart_for(&new_key, &login, false));
        assert!(running.needs_restart_for(&new_key, &login, true));
        let renamed = NodeConfig {
            hostname: Some("server-b".into()),
            ..running.clone()
        };
        assert!(running.needs_restart_for(&renamed, &connected, true));
    }

    #[test]
    fn a_port_opened_or_closed_restarts_the_node() {
        let running = config();
        let connected = Status::Connected {
            name: "server-a".into(),
            address: None,
        };
        let with_vnc = NodeConfig {
            forward_ports: vec![22, 5900],
            ..running.clone()
        };
        assert!(running.needs_restart_for(&with_vnc, &connected, true));
        assert!(with_vnc.needs_restart_for(&running, &connected, true));
        assert!(!with_vnc.needs_restart_for(&with_vnc.clone(), &connected, true));
        let without_ssh = NodeConfig {
            forward_ports: vec![5900],
            ..with_vnc.clone()
        };
        assert!(with_vnc.needs_restart_for(&without_ssh, &connected, true));
        let opened_elsewhere = NodeConfig {
            ports_opened_to: Some("example.ts.net/n2".into()),
            ..with_vnc.clone()
        };
        assert!(with_vnc.needs_restart_for(&opened_elsewhere, &connected, true));
    }

    #[test]
    fn ports_are_taken_in_only_by_the_node_they_were_opened_to() {
        let ports = [22, 5900, 5902];
        let here = "example.ts.net/n1";
        assert_eq!(taken_in(&ports, Some(here), here), [22, 5900, 5902]);
        // Let into another tailnet, or made another node in this one: SSH's
        // alone, as its settings say.
        assert_eq!(taken_in(&ports, Some(here), "other.ts.net/n1"), [22]);
        assert_eq!(taken_in(&ports, Some(here), "example.ts.net/n2"), [22]);
        // Opened before the node was known: none of them.
        assert_eq!(taken_in(&ports, None, here), [22]);
        assert!(taken_in(&[5900], Some("a/b"), "a/c").is_empty());
    }

    #[test]
    fn settings_a_node_cannot_start_on_are_caught_beforehand() {
        assert!(config().check().is_ok());
        let bad_url = NodeConfig {
            control_url: Some("https://".into()),
            ..config()
        };
        assert!(bad_url.check().is_err());
        let bad_keys = NodeConfig {
            node_keys: "{".into(),
            ..config()
        };
        assert!(bad_keys.check().is_err());
    }

    fn server_a() -> Peer {
        Peer {
            name: "server-a".into(),
            fqdn: "server-a.example.ts.net".into(),
            ipv4: Some(Ipv4Addr::new(100, 64, 0, 2)),
            ipv6: None,
            online: Some(true),
            ..Default::default()
        }
    }

    fn resolve_connected(host: &str) -> Option<IpAddr> {
        resolve_with(
            host,
            Family::Any,
            Duration::from_secs(5),
            &|| false,
            || Status::Connected {
                name: "example".into(),
                address: None,
            },
            || vec![server_a()],
            || None,
        )
    }

    #[test]
    fn the_selected_tailnet_resolves_only_its_own_machines() {
        let address = Some(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 2)));
        assert_eq!(resolve_connected("server-a"), address);
        assert_eq!(resolve_connected("server-a.example.ts.net."), address);
        assert_eq!(resolve_connected("100.64.0.2"), address);
        assert_eq!(resolve_connected("server-b"), None);
        assert_eq!(resolve_connected("100.64.0.9"), None);
        assert_eq!(resolve_connected("localhost"), None);
    }

    #[test]
    fn a_node_waiting_to_try_again_is_waited_for_too() {
        let in_after = Instant::now() + Duration::from_millis(300);
        let getting_there = || Instant::now() < in_after;
        let resolved = resolve_with(
            "server-a",
            Family::Any,
            Duration::from_secs(5),
            &|| false,
            || {
                if getting_there() {
                    Status::Failed {
                        error: "no network yet".into(),
                    }
                } else {
                    Status::Connected {
                        name: "devbox".into(),
                        address: None,
                    }
                }
            },
            || {
                if getting_there() {
                    vec![]
                } else {
                    vec![server_a()]
                }
            },
            || None,
        );
        assert_eq!(resolved, Some(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 2))));
    }

    #[test]
    fn a_selected_node_is_waited_for_until_connected_or_cancelled() {
        let in_after = Instant::now() + Duration::from_millis(300);
        let status = || {
            if Instant::now() < in_after {
                Status::Starting
            } else {
                Status::Connected {
                    name: "devbox".into(),
                    address: None,
                }
            }
        };
        let peers = || {
            if Instant::now() < in_after {
                vec![]
            } else {
                vec![server_a()]
            }
        };
        let wait = Duration::from_secs(5);
        let started = Instant::now();
        let resolved = resolve_with(
            "server-a",
            Family::Any,
            wait,
            &|| false,
            status,
            peers,
            || None,
        );
        assert_eq!(resolved, Some(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 2))));
        assert!(started.elapsed() >= Duration::from_millis(300));

        let starting = || Status::Starting;
        // A wait ends when the session is closed.
        let started = Instant::now();
        assert_eq!(
            resolve_with(
                "server-a",
                Family::Any,
                wait,
                &|| true,
                starting,
                Vec::new,
                || None,
            ),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_missing_machine_stops_waiting_at_the_deadline() {
        let started = Instant::now();
        assert_eq!(
            resolve_with(
                "server-a.example.com",
                Family::Any,
                Duration::ZERO,
                &|| false,
                || Status::Starting,
                Vec::new,
                || None,
            ),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn explicit_ipv6_is_preserved_for_a_dual_stack_peer() {
        let ipv6 = Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, 2);
        let mut peer = server_a();
        peer.ipv6 = Some(ipv6);
        let resolved = resolve_with(
            &ipv6.to_string(),
            Family::Any,
            Duration::from_secs(1),
            &|| false,
            || Status::Connected {
                name: "devbox".into(),
                address: None,
            },
            || vec![peer.clone()],
            || None,
        );
        assert_eq!(resolved, Some(IpAddr::V6(ipv6)));
    }

    #[test]
    fn a_connection_kept_to_one_family_goes_to_that_address() {
        let ipv6 = Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, 2);
        let mut dual_stack = server_a();
        dual_stack.ipv6 = Some(ipv6);
        let resolve = |host: &str, family: Family, peer: &Peer| {
            resolve_with(
                host,
                family,
                Duration::from_secs(1),
                &|| false,
                || Status::Connected {
                    name: "devbox".into(),
                    address: None,
                },
                || vec![peer.clone()],
                || None,
            )
        };
        let ipv4 = Some(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 2)));
        assert_eq!(resolve("server-a", Family::Any, &dual_stack), ipv4);
        assert_eq!(resolve("server-a", Family::V4, &dual_stack), ipv4);
        assert_eq!(
            resolve("server-a", Family::V6, &dual_stack),
            Some(IpAddr::V6(ipv6))
        );
        // A machine without an address of that family is not reached by it.
        assert_eq!(resolve("server-a", Family::V6, &server_a()), None);
        // Nor is an address of the other family.
        assert_eq!(resolve("100.64.0.2", Family::V6, &dual_stack), None);
        assert_eq!(resolve(&ipv6.to_string(), Family::V4, &dual_stack), None);
    }

    #[test]
    fn a_name_shared_with_a_machine_from_another_tailnet_means_this_ones() {
        let mut shared_in = server_a();
        shared_in.fqdn = "server-a.another.ts.net".into();
        shared_in.ipv4 = Some(Ipv4Addr::new(100, 64, 0, 9));
        let resolve = |host: &str, domain: Option<&str>| {
            resolve_with(
                host,
                Family::Any,
                Duration::from_secs(1),
                &|| false,
                || Status::Connected {
                    name: "devbox".into(),
                    address: None,
                },
                // Sorted by full name, the other tailnet's comes first.
                || vec![shared_in.clone(), server_a()],
                || domain.map(str::to_string),
            )
        };
        let own = Some(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 2)));
        let other = Some(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 9)));
        assert_eq!(resolve("server-a", Some("example.ts.net")), own);
        // Not knowing which tailnet is this one's, it guesses at neither.
        assert_eq!(resolve("server-a", None), None);
        // A full name says which.
        assert_eq!(resolve("server-a.another.ts.net", None), other);
    }

    #[tokio::test]
    async fn a_connected_node_can_need_login_and_be_authorized_again() {
        let wait = Duration::from_millis(20);
        assert_eq!(
            authorized_snapshot(
                async { Ok(Some(AuthState::Authorized)) },
                async { Ok(Some("before")) },
                wait
            )
            .await,
            Ok("before")
        );
        let login = "https://login.example.com/node";
        let status = authorized_snapshot(
            async { Ok(Some(AuthState::NotAuthorized(login.parse().unwrap()))) },
            std::future::pending::<Result<Option<&str>>>(),
            wait,
        )
        .await;
        assert_eq!(status, Err(Status::NeedsLogin { url: login.into() }));
        assert_eq!(
            authorized_snapshot(
                async { Ok(Some(AuthState::Authorized)) },
                async { Ok(Some("after")) },
                wait
            )
            .await,
            Ok("after")
        );
    }

    #[tokio::test]
    async fn a_missing_netmap_cannot_block_the_next_authorization_check() {
        let wait = Duration::from_millis(20);
        let pending = authorized_snapshot(
            async { Ok(Some(AuthState::Authorized)) },
            std::future::pending::<Result<Option<()>>>(),
            wait,
        )
        .await;
        assert_eq!(pending, Err(Status::Starting));
        let login = "https://login.example.com/again";
        assert_eq!(
            authorized_snapshot(
                async { Ok(Some(AuthState::NotAuthorized(login.parse().unwrap()))) },
                std::future::pending::<Result<Option<()>>>(),
                wait,
            )
            .await,
            Err(Status::NeedsLogin { url: login.into() })
        );
        assert_eq!(
            authorized_snapshot(
                std::future::pending::<Result<Option<AuthState>>>(),
                async { Ok(Some(())) },
                wait,
            )
            .await,
            Err(Status::Starting)
        );
    }

    #[tokio::test]
    async fn an_unanswered_question_is_waited_for_not_asked_again() {
        let wait = Duration::from_millis(20);
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let (tell, told) = tokio::sync::oneshot::channel();
        let mut told = Some(told);
        let mut ask = || {
            asked.fetch_add(1, Ordering::SeqCst);
            let told = told.take();
            async move {
                match told {
                    Some(told) => Ok(told.await?),
                    None => Ok(0),
                }
            }
        };
        let mut asking = None;
        for _ in 0..2 {
            let gave_up = tokio::time::timeout(wait, answer(&mut asking, &mut ask)).await;
            assert!(gave_up.is_err());
        }
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        tell.send(7).unwrap();
        assert_eq!(answer(&mut asking, &mut ask).await.unwrap(), 7);
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        // Once answered, the next one is asked anew.
        assert_eq!(answer(&mut asking, &mut ask).await.unwrap(), 0);
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn registration_and_netmap_not_ready_report_starting() {
        let wait = Duration::from_secs(1);
        assert_eq!(
            authorized_snapshot(
                async { Ok(None) },
                std::future::pending::<Result<Option<()>>>(),
                wait,
            )
            .await,
            Err(Status::Starting)
        );
        assert_eq!(
            authorized_snapshot(
                async { Ok(Some(AuthState::Authorized)) },
                async { Ok(None::<()>) },
                wait,
            )
            .await,
            Err(Status::Starting)
        );
    }

    #[test]
    fn new_keys_parse_as_the_library_reads_them() {
        let keys = new_node_keys().unwrap();
        let parsed: tailscale::keys::PersistState = serde_json::from_str(&keys).unwrap();
        assert_eq!(serde_json::to_string(&parsed).unwrap(), keys);
        assert_ne!(new_node_keys().unwrap(), keys);
    }

    #[test]
    fn a_machine_with_no_name_set_asks_for_one_made_from_its_key() {
        let unnamed = NodeConfig {
            hostname: None,
            ..config()
        };
        let keys: tailscale::keys::PersistState = serde_json::from_str(&unnamed.node_keys).unwrap();
        let name = automatic_name(&keys);
        let digits = name.strip_prefix("thinkterm-").unwrap();
        assert_eq!(digits.len(), 4);
        assert!(digits
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        let asked = unnamed.library_config().unwrap().requested_hostname;
        assert_eq!(asked, Some(name));
        let named = config().library_config().unwrap();
        assert_eq!(named.requested_hostname.as_deref(), Some("server-a"));
        // Made the same way in every version: a change would rename every
        // machine that has no name set.
        let fixed = tailscale::keys::PersistState {
            machine_key: tailscale::keys::Export::from_bytes([7; 32]),
            ..Default::default()
        };
        assert_eq!(automatic_name(&fixed), "thinkterm-13be");
    }

    #[tokio::test]
    async fn bridge_carries_both_ways_and_ends_with_either_side() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut left, left_inner) = tokio::io::duplex(64);
        let (right_inner, mut right) = tokio::io::duplex(64);
        let bridged = tokio::spawn(bridge(left_inner, right_inner));

        left.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        right.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        right.write_all(b"pong").await.unwrap();
        left.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");

        drop(left);
        bridged.await.unwrap();
        let mut rest = vec![];
        right.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    }

    /// What a connection handed to `port` here carries both ways, as the
    /// server on `local` sees it, and that it ends with the server's side.
    async fn handed_over(local: tokio::net::TcpListener) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let port = local.local_addr().unwrap().port();
        let (mut tailnet, came_in) = tokio::io::duplex(64);
        let handed = tokio::spawn(hand_to_this_machine(came_in, port));
        // Failing, not waiting forever, should it have gone elsewhere.
        let (mut server, _) = tokio::time::timeout(Duration::from_secs(5), local.accept())
            .await
            .expect("the connection reached the server")
            .unwrap();

        tailnet.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        server.write_all(b"pong").await.unwrap();
        tailnet.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");

        drop(server);
        tokio::time::timeout(Duration::from_secs(5), handed)
            .await
            .unwrap()
            .unwrap();
        let mut rest = vec![];
        tailnet.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
    }

    #[tokio::test]
    async fn a_connection_from_the_tailnet_is_handed_to_the_same_port_here() {
        handed_over(
            tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap(),
        )
        .await;
    }

    #[tokio::test]
    async fn a_client_that_sends_its_end_still_gets_the_answer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let local = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = local.local_addr().unwrap().port();
        let (mut tailnet, came_in) = tokio::io::duplex(64);
        let handed = tokio::spawn(hand_to_this_machine(came_in, port));
        // A server that answers only once the request has ended.
        let server = tokio::spawn(async move {
            let (mut server, _) = local.accept().await.unwrap();
            let mut request = vec![];
            server.read_to_end(&mut request).await.unwrap();
            server.write_all(b"answer to ").await.unwrap();
            server.write_all(&request).await.unwrap();
        });
        tailnet.write_all(b"question").await.unwrap();
        tailnet.shutdown().await.unwrap();
        let mut answer = vec![];
        tokio::time::timeout(Duration::from_secs(5), tailnet.read_to_end(&mut answer))
            .await
            .expect("the answer came back")
            .unwrap();
        assert_eq!(answer, b"answer to question");
        server.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), handed)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_server_silent_after_the_end_is_let_go() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let local = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = local.local_addr().unwrap().port();
        let connect = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port));
        let (connected, accepted) = tokio::join!(connect, local.accept());
        let (mut server, _) = accepted.unwrap();
        let (mut tailnet, came_in) = tokio::io::duplex(64);
        let bridged = tokio::spawn(bridge_to_local(
            came_in,
            connected.unwrap(),
            Duration::from_millis(200),
        ));
        tailnet.write_all(b"bye").await.unwrap();
        tailnet.shutdown().await.unwrap();
        // The server reads the end and then keeps the connection, silent.
        let mut request = vec![];
        server.read_to_end(&mut request).await.unwrap();
        assert_eq!(request, b"bye");
        tokio::time::timeout(Duration::from_secs(5), bridged)
            .await
            .expect("let go after being quiet that long")
            .unwrap();
        let mut rest = vec![];
        tailnet.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
        drop(server);
    }

    #[tokio::test]
    async fn a_server_on_ipv6_loopback_alone_takes_it_too() {
        // A machine without IPv6 loopback has nothing to tell here.
        let Ok(local) = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).await else {
            return;
        };
        // Nothing may answer on IPv4's at that port, or it would take it:
        // held, not listened on, it refuses connections meanwhile and
        // nothing else can take it.
        let port = local.local_addr().unwrap().port();
        let held = tokio::net::TcpSocket::new_v4().unwrap();
        if held.bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).is_err() {
            return;
        }
        handed_over(local).await;
        drop(held);
    }

    #[tokio::test]
    async fn a_connection_nothing_here_answers_ends_at_once() {
        use tokio::io::AsyncReadExt;
        // A port held on both loopbacks and listened on by neither: both
        // refuse it meanwhile, and nothing else can take it.
        let v4 = tokio::net::TcpSocket::new_v4().unwrap();
        v4.bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let port = v4.local_addr().unwrap().port();
        let v6 = match tokio::net::TcpSocket::new_v6() {
            Ok(v6) => match v6.bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port))) {
                Ok(()) => Some(v6),
                // Someone else's: it might answer.
                Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => return,
                // No IPv6 loopback here: nothing answers on it either.
                Err(_) => None,
            },
            Err(_) => None,
        };
        let (mut tailnet, came_in) = tokio::io::duplex(64);
        tokio::time::timeout(
            Duration::from_secs(5),
            hand_to_this_machine(came_in, port),
        )
        .await
        .unwrap();
        let mut rest = vec![];
        tailnet.read_to_end(&mut rest).await.unwrap();
        assert!(rest.is_empty());
        drop((v4, v6));
    }

    /// Shared state with nothing running, and how often it said it changed.
    fn shared_counting() -> (Arc<Shared>, Arc<std::sync::atomic::AtomicUsize>) {
        let said = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let shared = Arc::new(Shared {
            state: Mutex::new((Status::Starting, vec![])),
            device: Mutex::new(None),
            domain: Mutex::new(None),
            membership: Mutex::new(None),
            listening: Mutex::new(std::collections::BTreeSet::new()),
            stopped: AtomicBool::new(false),
            broken: AtomicBool::new(false),
            was_in: AtomicBool::new(false),
            problem: Mutex::new(None),
            changed: Box::new({
                let said = Arc::clone(&said);
                move || {
                    said.fetch_add(1, Ordering::SeqCst);
                }
            }),
        });
        (shared, said)
    }

    #[test]
    fn a_node_counts_as_stuck_only_when_stuck_throughout() {
        let long = Duration::from_secs(30);
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        let mut stuck = Stuck::default();
        assert!(!stuck.for_at_least(true, long, at(0)));
        assert!(!stuck.for_at_least(true, long, at(29)));
        // One look that finds it in starts it over.
        assert!(!stuck.for_at_least(false, long, at(30)));
        assert!(!stuck.for_at_least(true, long, at(31)));
        assert!(!stuck.for_at_least(true, long, at(60)));
        assert!(stuck.for_at_least(true, long, at(61)));
    }

    #[test]
    fn a_part_of_the_library_gone_gives_a_node_that_was_in_up_at_once() {
        use tailscale::InternalErrorKind::{Actor, InternalChannelClosed};
        assert!(library_gone(&tailscale::Error::Internal(Actor)));
        assert!(library_gone(&tailscale::Error::Internal(InternalChannelClosed)));
        // The machine turning a connection away is no part of the library.
        assert!(!library_gone(&tailscale::Error::ConnectionReset));

        // Never in: left to the watchdog, the error passed on as it was.
        let (shared, _) = shared_counting();
        let err = shared.library_failed(tailscale::Error::Internal(Actor));
        assert!(!shared.is_going());
        assert_eq!(err.to_string(), tailscale::Error::Internal(Actor).to_string());

        // In before: given up at once, to be made anew.
        let (shared, said) = shared_counting();
        shared.was_in.store(true, Ordering::SeqCst);
        shared.library_failed(tailscale::Error::Internal(Actor));
        assert!(shared.is_going());
        assert_eq!(shared.status(), Status::Failed { error: STARTING_ANEW.into() });
        assert!(said.load(Ordering::SeqCst) > 0, "the holder is told");

        // Another failure gives nothing up.
        let (shared, _) = shared_counting();
        shared.was_in.store(true, Ordering::SeqCst);
        shared.library_failed(tailscale::Error::ConnectionReset);
        assert!(!shared.is_going());
    }

    #[test]
    fn a_node_short_of_in_or_a_login_is_stuck() {
        assert!(is_stuck(&Status::Starting));
        assert!(is_stuck(&Status::Failed {
            error: "stopped".into(),
        }));
        assert!(!is_stuck(&Status::Connected {
            name: "devbox".into(),
            address: None,
        }));
        assert!(!is_stuck(&Status::NeedsLogin {
            url: "https://login.example.com/node".into(),
        }));
        // Waiting for an admin: the library asks again by itself.
        assert!(!is_stuck(&Status::Failed {
            error: crate::NOT_LET_IN.into(),
        }));
    }

    /// Whether `shared` is given up within `wait`.
    async fn given_up_within(shared: &Shared, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            if shared.broken.load(Ordering::SeqCst) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        shared.broken.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn the_watchdog_gives_up_a_node_that_never_got_in() {
        let long = Duration::from_millis(300);
        let (shared, _) = shared_counting();
        tokio::spawn(watchdog(Arc::clone(&shared), long, || async {
            unreachable!("asked about the control server")
        }));
        assert!(given_up_within(&shared, Duration::from_secs(5)).await);
        assert_eq!(shared.status(), Status::Starting);
    }

    #[tokio::test]
    async fn the_watchdog_leaves_a_node_waiting_for_a_login() {
        let long = Duration::from_millis(300);
        let (shared, _) = shared_counting();
        shared.set_status(Status::NeedsLogin {
            url: "https://login.example.com/node".into(),
        });
        tokio::spawn(watchdog(Arc::clone(&shared), long, || async { true }));
        assert!(!given_up_within(&shared, long * 3).await);
    }

    #[tokio::test]
    async fn a_node_that_was_in_is_given_up_once_its_control_server_answers() {
        let long = Duration::from_millis(300);
        let (shared, _) = shared_counting();
        shared.set_status(Status::Connected {
            name: "devbox".into(),
            address: None,
        });
        let answers = Arc::new(AtomicBool::new(false));
        tokio::spawn(watchdog(Arc::clone(&shared), long, {
            let answers = Arc::clone(&answers);
            move || {
                let answers = Arc::clone(&answers);
                async move { answers.load(Ordering::SeqCst) }
            }
        }));
        tokio::time::sleep(long / 10).await;
        // Getting back to a control server that is away: one started in its
        // place could not get in either.
        shared.set_status(Status::Starting);
        assert!(!given_up_within(&shared, long * 3).await);
        // Part of the library stopped, with the control server still away.
        shared.set_status(Status::Failed {
            error: "stopped".into(),
        });
        assert!(!given_up_within(&shared, long * 3).await);
        answers.store(true, Ordering::SeqCst);
        assert!(given_up_within(&shared, Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn a_node_that_was_in_and_never_gets_back_is_given_up() {
        let long = Duration::from_millis(300);
        let (shared, _) = shared_counting();
        shared.set_status(Status::Connected {
            name: "devbox".into(),
            address: None,
        });
        tokio::spawn(watchdog(Arc::clone(&shared), long, || async { true }));
        tokio::time::sleep(long / 10).await;
        // On a control connection that went with the network it was made
        // on, with the control server answering anew.
        shared.set_status(Status::Starting);
        assert!(given_up_within(&shared, Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn a_node_that_gets_in_while_its_control_server_is_asked_is_left() {
        let long = Duration::from_millis(300);
        let (shared, _) = shared_counting();
        let connected = Status::Connected {
            name: "devbox".into(),
            address: None,
        };
        shared.set_status(connected.clone());
        tokio::spawn(watchdog(Arc::clone(&shared), long, {
            let shared = Arc::clone(&shared);
            move || {
                let shared = Arc::clone(&shared);
                let connected = connected.clone();
                async move {
                    shared.set_status(connected);
                    true
                }
            }
        }));
        tokio::time::sleep(long / 10).await;
        shared.set_status(Status::Failed {
            error: "stopped".into(),
        });
        assert!(!given_up_within(&shared, long * 3).await);
    }

    #[test]
    fn nodes_given_up_in_a_row_wait_longer_up_to_two_minutes() {
        let waits: Vec<u64> = (0..5).map(|n| broken_after(n).as_secs()).collect();
        assert_eq!(waits, [30, 60, 120, 120, 120]);
    }

    #[test]
    fn giving_up_says_so_even_with_the_status_unchanged() {
        let (shared, said) = shared_counting();
        let failed = Status::Failed {
            error: "stopped".into(),
        };
        shared.set_status(failed.clone());
        assert_eq!(said.load(Ordering::SeqCst), 1);
        shared.give_up(failed.clone());
        assert!(shared.broken.load(Ordering::SeqCst));
        assert_eq!(shared.status(), failed);
        assert_eq!(said.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_node_that_got_in_meanwhile_is_not_given_up() {
        let (shared, said) = shared_counting();
        shared.set_status(Status::Connected {
            name: "devbox".into(),
            address: None,
        });
        let before = said.load(Ordering::SeqCst);
        assert!(!shared.give_up_if_stuck());
        assert!(!shared.broken.load(Ordering::SeqCst));
        assert_eq!(said.load(Ordering::SeqCst), before);
        // Stuck still: given up, and said so with the status as it was.
        let (shared, said) = shared_counting();
        assert!(shared.give_up_if_stuck());
        assert!(shared.broken.load(Ordering::SeqCst));
        assert_eq!(shared.status(), Status::Starting);
        assert_eq!(said.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_loop_that_panics_gives_the_node_up() {
        let (shared, _) = shared_counting();
        let runtime = tokio::runtime::Handle::current();
        watched(&runtime, &shared, std::future::pending());
        tokio::task::yield_now().await;
        assert!(!shared.broken.load(Ordering::SeqCst));
        watched(&runtime, &shared, async { panic!("a loop of the node") });
        for _ in 0..100 {
            if shared.broken.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(shared.broken.load(Ordering::SeqCst));
        assert!(matches!(shared.status(), Status::Failed { .. }));
    }
}
