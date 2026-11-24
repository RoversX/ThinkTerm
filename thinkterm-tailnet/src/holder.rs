//! Holding the machine's node: what `thinkterm-tailnet` does, from the time
//! it starts following the settings ([`follow`]). The node starts when the
//! settings turn the tailnet on, restarts when they change what it runs on,
//! and stops when they turn it off; what it does goes to the report the app
//! and `thinkterm tailnet` read.

use crate::node;
use crate::settings::{self, Settings};
use crate::status::{self, read_live_report, read_report, Peer, Report, Status};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How often the settings file is looked at for a change made by another
/// process -- the app's switch, `thinkterm tailnet up` -- and the longest a
/// process kept from the node by another one waits to try again.
const POLL: Duration = Duration::from_secs(3);

/// The first wait of a process kept from the node by another one, which
/// doubles up to POLL: the other may be on its way out, as the
/// predecessor's is when a mux server takes over.
const HELD_FIRST: Duration = Duration::from_millis(250);

/// The longest wait before trying again at a node that would not start.
const RETRY_MAX: Duration = Duration::from_secs(60);

type Changed = Arc<dyn Fn() + Send + Sync>;

/// Why the settings thread looks again before its next poll.
enum Wake {
    /// This process changed the settings file.
    Settings,
    /// The node's status or its list of machines changed.
    Node,
}

struct Follower {
    wake: Mutex<mpsc::Sender<Wake>>,
}

static FOLLOWER: OnceLock<Follower> = OnceLock::new();

/// The status while no node runs here: Off, or why there is none.
static IDLE: Mutex<Status> = Mutex::new(Status::Off);

/// Whether the settings thread last found the tailnet off, and let go of
/// the node for it (see [`is_off`]).
static OFF: AtomicBool = AtomicBool::new(false);

/// Whether the settings thread runs (see [`is_following`]).
static FOLLOWING: AtomicBool = AtomicBool::new(false);

/// Says the settings thread has ended however it ends, a panic included.
struct FollowingEnds;

impl Drop for FollowingEnds {
    fn drop(&mut self) {
        FOLLOWING.store(false, Ordering::SeqCst);
    }
}

/// The settings' revision this process last acted on, for its reports.
static ACTED: AtomicU64 = AtomicU64::new(0);

static NODE: Mutex<Option<Arc<node::Node>>> = Mutex::new(None);

/// The nodes in a row this process gave up and started anew (see
/// `node::Node::is_broken`) since one last got in or as far as a login: each
/// waits longer before it is given up.
static GIVEN_UP: AtomicU32 = AtomicU32::new(0);

/// The lock this process holds the machine's node by (see
/// `settings::try_hold_node`), from before its node starts until after the
/// node is gone and said to be.
static HOLD: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Holds the machine's node in this process for as long as the settings
/// file turns the tailnet on, starting, stopping and restarting it as the
/// file changes. `changed` is called, on a background thread, whenever the
/// file, the status or the list of machines changes. Calling it again does
/// nothing.
pub fn follow(changed: impl Fn() + Send + Sync + 'static) {
    let changed: Changed = Arc::new(changed);
    let (wake, woken) = mpsc::channel();
    if FOLLOWER
        .set(Follower {
            wake: Mutex::new(wake),
        })
        .is_err()
    {
        return;
    }
    FOLLOWING.store(true, Ordering::SeqCst);
    let spawned = std::thread::Builder::new()
        .name("tailnet-settings".into())
        .spawn(move || {
            let _ends = FollowingEnds;
            follow_loop(changed, woken)
        });
    if let Err(err) = spawned {
        FOLLOWING.store(false, Ordering::SeqCst);
        log::error!("tailnet: starting the settings thread: {err}");
    }
}

/// Whether the settings thread runs: from [`follow`] until it ends, by a
/// panic among others. Nothing acts on the settings after that, and the
/// node, if there is one, goes on as it was: `thinkterm-tailnet` ends for
/// the mux server to start it anew.
pub fn is_following() -> bool {
    FOLLOWING.load(Ordering::SeqCst)
}

/// Whether the settings thread last found the tailnet off and let go of the
/// node for it: what `thinkterm-tailnet` ends at. False until it first
/// looked, and while it cannot read the settings.
pub fn is_off() -> bool {
    OFF.load(Ordering::SeqCst)
}

/// This process changed the settings file: the settings thread looks again
/// at once, rather than at its next poll.
pub(crate) fn settings_changed() {
    if FOLLOWER.get().is_none() {
        return;
    }
    send(Wake::Settings);
}

/// Whether the settings thread got it: not once it has ended.
fn send(wake: Wake) -> bool {
    FOLLOWER
        .get()
        .is_some_and(|follower| follower.wake.lock().unwrap().send(wake).is_ok())
}

/// Whether this process's node was given up (see `node::Node::is_broken`).
fn node_is_broken() -> bool {
    NODE.lock()
        .unwrap()
        .as_ref()
        .is_some_and(|node| node.is_broken())
}

/// This process's node as last seen.
fn status() -> Status {
    if let Some(node) = NODE.lock().unwrap().as_ref() {
        return node.status();
    }
    IDLE.lock().unwrap().clone()
}

/// The other machines in the tailnet, by name.
fn peers() -> Vec<Peer> {
    if let Some(node) = NODE.lock().unwrap().as_ref() {
        return node.peers();
    }
    vec![]
}

/// The ports this process's node listens on for the tailnet, and the node
/// as its tailnet knows it: none and nobody without one.
fn forwarding() -> (Vec<u16>, Option<String>) {
    NODE.lock()
        .unwrap()
        .as_ref()
        .map(|node| (node.forwarding(), node.membership()))
        .unwrap_or_default()
}

/// What a report says: the node's status, its machines, the ports it takes
/// in and the node as its tailnet knows it.
type Told = (Status, Vec<Peer>, (Vec<u16>, Option<String>));

/// What the settings thread does after acting on the settings.
enum Next {
    /// Nothing until the settings change.
    Settled,
    /// Another process holds the node: try again at the next poll.
    Held,
    /// It went wrong: try again after a while, a longer one each time.
    Failed,
}

fn follow_loop(changed: Changed, woken: mpsc::Receiver<Wake>) {
    let path = match settings::path() {
        Ok(path) => path,
        Err(err) => {
            log::error!("tailnet: {err:#}");
            return;
        }
    };
    let mut seen = None;
    let mut look = true;
    // When to try again, and how long the wait before that was.
    let mut retry: Option<(Instant, Duration)> = None;
    let mut told = None;
    loop {
        let stamp = settings::stamp(&path);
        let file_changed = look || stamp != seen;
        if file_changed || retry.is_some_and(|(at, _)| Instant::now() >= at) {
            seen = stamp;
            retry = match reconcile() {
                Next::Settled => None,
                Next::Held => {
                    let wait = held_wait(retry.map(|(_, wait)| wait));
                    Some((Instant::now() + wait, wait))
                }
                Next::Failed => {
                    let wait = retry.map_or(POLL, |(_, wait)| (wait * 2).clamp(POLL, RETRY_MAX));
                    Some((Instant::now() + wait, wait))
                }
            };
        }
        // Told when it changes, and after every change of the file even when
        // the node goes on as it was: the page shows the file, and
        // `thinkterm tailnet up` waits for a report written after its
        // change.
        let now: Told = (status(), peers(), forwarding());
        if matches!(now.0, Status::Connected { .. } | Status::NeedsLogin { .. }) {
            GIVEN_UP.store(0, Ordering::SeqCst);
        }
        if file_changed || told.as_ref() != Some(&now) {
            report(&now);
            changed();
            told = Some(now);
        }
        // Not past the next try, which may be sooner than the next poll.
        let sleep = retry.map_or(POLL, |(at, _)| {
            at.saturating_duration_since(Instant::now()).min(POLL)
        });
        look = match woken.recv_timeout(sleep) {
            Ok(Wake::Settings) => true,
            // A node that has to get in again may take an auth key saved
            // while it was in, which did not restart it then; one given up
            // is started anew at once, even one that got in at the last
            // moment, not at whatever look comes next.
            Ok(Wake::Node) => node_is_broken() || !matches!(status(), Status::Connected { .. }),
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
    }
}

/// The wait before trying again at a node another process holds, after
/// `previous`.
fn held_wait(previous: Option<Duration>) -> Duration {
    previous.map_or(HELD_FIRST, |wait| (wait * 2).min(POLL))
}

/// Starts, stops or restarts this process's node to match the settings.
///
/// The slow parts -- writing new keys, building the node's runtime -- run
/// without the NODE lock, which a dial takes.
fn reconcile() -> Next {
    // Without the secrets first: most looks change nothing.
    let summary = match settings::load_summary() {
        Ok(summary) => summary,
        Err(err) => return failed(err),
    };
    ACTED.store(summary.revision, Ordering::SeqCst);
    OFF.store(false, Ordering::SeqCst);
    if !summary.enabled {
        let node = NODE.lock().unwrap().take();
        let stopped = node.is_some();
        if let Some(node) = node {
            node.stop();
        }
        let was = std::mem::replace(&mut *IDLE.lock().unwrap(), Status::Off);
        let held = HOLD.lock().unwrap().is_some();
        if held && (stopped || matches!(was, Status::Failed { .. })) {
            report_if_ours(Status::Off);
        }
        // Only after the report: the process that takes the node over can
        // only start once this lock is free, so its report comes after.
        HOLD.lock().unwrap().take();
        OFF.store(true, Ordering::SeqCst);
        return Next::Settled;
    }

    {
        let mut hold = HOLD.lock().unwrap();
        if hold.is_none() {
            match settings::try_hold_node() {
                Ok(Some(lock)) => *hold = Some(lock),
                Ok(None) => {
                    drop(hold);
                    *IDLE.lock().unwrap() = Status::HeldElsewhere;
                    return Next::Held;
                }
                Err(err) => {
                    drop(hold);
                    return failed(err);
                }
            }
        }
    }

    let settings = match settings::load() {
        Ok(settings) => settings,
        Err(err) => return failed(err),
    };
    // The machine's identity is made on its first start and kept from then
    // on, so that it stays one node across restarts. Made only while the
    // file still turns the tailnet on -- not after a Log out landed since
    // the file was read -- and the node then starts on the file as it is.
    let settings = if settings.node_keys.is_some() {
        settings
    } else {
        match settings_with_node_keys() {
            Ok(Some(settings)) => settings,
            // Turned off meanwhile: the look the change brings acts on it.
            Ok(None) => return Next::Settled,
            Err(err) => return failed(err),
        }
    };
    let Some(node_keys) = settings.node_keys.clone() else {
        return failed(anyhow::anyhow!("the node's keys were not saved"));
    };
    // Compared in the one spelling, an address written by hand included.
    let control_url = match settings
        .control_url
        .as_deref()
        .map(crate::control_url_as_kept)
    {
        Some(Ok(url)) => url,
        Some(Err(err)) => return unusable(err),
        None => None,
    };
    let config = node::NodeConfig {
        control_url,
        hostname: settings.hostname.clone(),
        auth_key: settings.auth_key.clone(),
        node_keys,
        forward_ports: settings.forwarded_ports(),
        ports_opened_to: settings.serve_ports_to.clone(),
    };
    if let Err(err) = config.check() {
        return unusable(err);
    }
    let (restart, broken) = match NODE.lock().unwrap().as_ref() {
        Some(node) if node.is_broken() => (true, true),
        Some(node) => {
            let restart = node
                .config()
                .needs_restart_for(&config, &node.status(), node.was_in());
            (restart, false)
        }
        None => (true, false),
    };
    if !restart {
        return Next::Settled;
    }
    if broken {
        GIVEN_UP.fetch_add(1, Ordering::SeqCst);
    }
    // One node on these keys at a time: the old one goes first.
    let old = NODE.lock().unwrap().take();
    if let Some(old) = old {
        old.stop();
    }
    match node::Node::start(
        config,
        node::broken_after(GIVEN_UP.load(Ordering::SeqCst)),
        Box::new(|| {
            send(Wake::Node);
        }),
    ) {
        Ok(node) => {
            // A node the other processes cannot reach still serves this one.
            match crate::proxy::bind() {
                Ok((listener, path)) => {
                    node.spawn(crate::proxy::serve(node.shared(), listener));
                    node.at_stop(move || {
                        let _ = std::fs::remove_file(&path);
                    });
                }
                Err(err) => log::warn!(
                    "tailnet: sharing the node with this machine's other processes: {err:#}"
                ),
            }
            *NODE.lock().unwrap() = Some(Arc::new(node));
            *IDLE.lock().unwrap() = Status::Off;
            Next::Settled
        }
        Err(err) => failed(err),
    }
}

/// Settings no node can start on -- the file edited by hand -- leave a
/// running one as it was, rather than take the machine off the tailnet
/// until someone gets to it some other way. Not tried again until the file
/// changes.
fn unusable(err: anyhow::Error) -> Next {
    failed(err.context("not acting on the tailnet settings"));
    Next::Settled
}

/// Says why there is no node; one already running goes on as it was.
fn failed(err: anyhow::Error) -> Next {
    log::warn!("tailnet: {err:#}");
    if NODE.lock().unwrap().is_none() {
        *IDLE.lock().unwrap() = Status::Failed {
            error: format!("{err:#}"),
        };
    }
    Next::Failed
}

/// Writes new keys to the settings file while it still turns the tailnet
/// on, unless another process got there first, and returns the settings as
/// the file holds them now. None once the tailnet is off.
fn settings_with_node_keys() -> Result<Option<Settings>> {
    let mut made = None;
    let settings = crate::update(|settings| {
        if settings.enabled && settings.node_keys.is_none() {
            settings.node_keys = node::new_node_keys().map_err(|err| made = Some(err)).ok();
        }
    })?;
    if !settings.enabled {
        return Ok(None);
    }
    if let Some(err) = made {
        return Err(err);
    }
    Ok(Some(settings))
}

/// Writes what the node is doing for the app and `thinkterm tailnet` to
/// read: this process's node, or why it has none -- held elsewhere, failed
/// before it got the node -- unless another live process reports here.
fn report((status, peers, (forwarding, membership)): &Told) {
    let holding = HOLD.lock().unwrap().is_some();
    if !holding && read_live_report().is_some_and(|report| report.pid != std::process::id()) {
        return;
    }
    // Nothing to say where the tailnet has never been on.
    if !holding && *status == Status::Off && read_report().is_none() {
        return;
    }
    status::write_report(&Report {
        pid: std::process::id(),
        status: status.clone(),
        peers: peers.clone(),
        revision: ACTED.load(Ordering::SeqCst),
        forwarding: Some(forwarding.clone()),
        membership: membership.clone(),
    });
}

/// Says this process's node is gone, unless the report is already another
/// process's.
fn report_if_ours(status: Status) {
    if read_report().is_some_and(|report| report.pid != std::process::id()) {
        return;
    }
    status::write_report(&Report {
        pid: std::process::id(),
        status,
        peers: vec![],
        revision: ACTED.load(Ordering::SeqCst),
        forwarding: Some(vec![]),
        membership: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settings_thread_that_panics_is_told_to_have_ended() {
        FOLLOWING.store(true, Ordering::SeqCst);
        let ended = std::thread::spawn(|| {
            let _ends = FollowingEnds;
            panic!("the settings thread failing on purpose");
        })
        .join();
        assert!(ended.is_err());
        assert!(!is_following());
    }

    #[test]
    fn a_node_held_elsewhere_is_tried_again_soon_at_first() {
        let mut waits = vec![];
        let mut wait = None;
        for _ in 0..6 {
            let next = held_wait(wait);
            waits.push(next.as_millis());
            wait = Some(next);
        }
        assert_eq!(waits, [250, 500, 1000, 2000, 3000, 3000]);
        // After a start that failed, at most a poll away.
        assert_eq!(held_wait(Some(RETRY_MAX)), POLL);
    }
}
