//! The connected clients, and the plugins they call.
//!
//! Each client has a thread reading its calls and one writing what it is
//! sent, from a queue of its own: a client that stops reading loses its
//! connection instead of holding up everyone else's events.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::Shutdown;
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use thinkterm_plugin_channel::wire::{read_frame, write_frame, FromHost, ToHost, PROTOCOL};
use wezterm_uds::UnixStream;

/// Frames that may wait for one client. A client this far behind is not
/// reading, and is disconnected.
const QUEUE: usize = 256;
/// The threads here parse JSON and move bytes; a host with two clients has
/// no use for megabytes of stack each.
const STACK: usize = 256 * 1024;

/// A plugin as the host runs it.
pub trait Plugin: Send {
    fn name(&self) -> &'static str;

    /// Answers `body`, a call from a client. The plugin may make the caller
    /// one of its watchers and leave events for all of them in `outcome`.
    fn call(&mut self, body: Value, outcome: &mut Outcome) -> anyhow::Result<Value>;
}

#[derive(Default)]
pub struct Outcome {
    /// The caller is sent this plugin's events from now on.
    pub watch: bool,
    /// Sent to every watcher, the caller included, after the answer.
    pub events: Vec<Value>,
}

struct Client {
    outbox: SyncSender<Vec<u8>>,
    /// Shut down to drop a client whose writer may be stuck in a write.
    socket: UnixStream,
    watching: HashSet<&'static str>,
}

struct State {
    plugins: Vec<Box<dyn Plugin>>,
    clients: HashMap<u64, Client>,
    next_client: u64,
    /// When the last client left, or the host started; `None` while anyone
    /// is connected.
    idle_since: Option<Instant>,
}

pub struct Host {
    state: Mutex<State>,
    socket: PathBuf,
    idle: Duration,
}

impl Host {
    pub fn new(socket: PathBuf, idle: Duration, plugins: Vec<Box<dyn Plugin>>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                plugins,
                clients: HashMap::new(),
                next_client: 1,
                idle_since: Some(Instant::now()),
            }),
            socket,
            idle,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A plugin that panicked mid-call leaves its own data as it was;
        // the table of clients is updated only here, between calls.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn accept(self: &Arc<Self>, stream: UnixStream) {
        let (mut writer, socket) = match stream
            .try_clone()
            .and_then(|w| Ok((w, stream.try_clone()?)))
        {
            Ok(handles) => handles,
            Err(err) => {
                log::warn!("taking more handles on a client: {err}");
                return;
            }
        };
        let (outbox, queued) = sync_channel::<Vec<u8>>(QUEUE);
        let _ = outbox.try_send(FromHost::Hello { protocol: PROTOCOL }.encode());
        let id = {
            let mut state = self.state();
            let id = state.next_client;
            state.next_client += 1;
            state.clients.insert(
                id,
                Client {
                    outbox,
                    socket,
                    watching: HashSet::new(),
                },
            );
            state.idle_since = None;
            id
        };

        let written = std::thread::Builder::new()
            .name(format!("client-{id}-out"))
            .stack_size(STACK)
            .spawn(move || {
                for frame in queued {
                    if write_frame(&mut writer, &frame).is_err() {
                        break;
                    }
                }
                // Dropped from the table, or gone: either way the reader
                // is done with it too.
                let _ = writer.shutdown(Shutdown::Both);
            });
        let host = Arc::clone(self);
        let reading = std::thread::Builder::new()
            .name(format!("client-{id}-in"))
            .stack_size(STACK)
            .spawn(move || {
                host.serve(id, stream);
                host.leave(id);
            });
        if let Err(err) = written.and(reading) {
            log::warn!("starting the threads for a client: {err}");
            self.leave(id);
        }
    }

    fn serve(&self, client: u64, mut stream: UnixStream) {
        loop {
            let frame = match read_frame(&mut stream) {
                Ok(frame) => frame,
                Err(_) => return,
            };
            match ToHost::decode(&frame) {
                Ok(ToHost::Call { id, plugin, body }) => self.call(client, id, &plugin, body),
                Ok(ToHost::Quit) => {
                    log::info!("asked to quit by a newer build");
                    // Once any call in progress has finished.
                    self.exit(self.state());
                }
                Err(err) => {
                    log::warn!("client {client} sent something that is not a message: {err}");
                    return;
                }
            }
        }
    }

    fn call(&self, client: u64, id: u64, plugin: &str, body: Value) {
        let mut state = self.state();
        let state = &mut *state;
        let mut outcome = Outcome::default();
        let (name, answer) = match state.plugins.iter_mut().find(|p| p.name() == plugin) {
            Some(found) => (found.name(), found.call(body, &mut outcome)),
            None => {
                let answer = FromHost::Error {
                    id,
                    message: format!("there is no plugin named {plugin:?}"),
                };
                deliver(&mut state.clients, client, answer.encode());
                return;
            }
        };
        let answer = match answer {
            Ok(body) => FromHost::Ok { id, body },
            Err(err) => FromHost::Error {
                id,
                message: format!("{err:#}"),
            },
        };
        if outcome.watch {
            if let Some(caller) = state.clients.get_mut(&client) {
                caller.watching.insert(name);
            }
        }
        deliver(&mut state.clients, client, answer.encode());
        for body in outcome.events {
            let frame = FromHost::Event {
                plugin: name.to_string(),
                body,
            }
            .encode();
            let watchers: Vec<u64> = state
                .clients
                .iter()
                .filter(|(_, watcher)| watcher.watching.contains(name))
                .map(|(id, _)| *id)
                .collect();
            for watcher in watchers {
                deliver(&mut state.clients, watcher, frame.clone());
            }
        }
        if state.clients.is_empty() {
            state.idle_since.get_or_insert_with(Instant::now);
        }
    }

    fn leave(&self, client: u64) {
        let mut state = self.state();
        state.clients.remove(&client);
        if state.clients.is_empty() {
            state.idle_since.get_or_insert_with(Instant::now);
        }
    }

    /// Exits once nobody has been connected for the idle time.
    pub fn exit_when_idle(self: &Arc<Self>) {
        let host = Arc::clone(self);
        let reaper = std::thread::Builder::new()
            .name("idle".into())
            .stack_size(STACK)
            .spawn(move || loop {
                let wait = {
                    // Decided with the table locked, so a client being
                    // accepted either registers first or finds the socket
                    // gone and starts a new host.
                    let state = host.state();
                    match state.idle_since {
                        Some(since) if since.elapsed() >= host.idle => {
                            log::info!("nobody connected for {:?}; exiting", host.idle);
                            host.exit(state);
                        }
                        Some(since) => host.idle - since.elapsed(),
                        None => host.idle,
                    }
                };
                std::thread::sleep(wait);
            });
        if let Err(err) = reaper {
            log::warn!("cannot watch for idleness, so this host stays up: {err}");
        }
    }

    /// Removes the socket, so the next client starts a new host rather
    /// than finding a dead one, and exits. Every change is on disk by the
    /// time it is answered, and holding the table means no call is under
    /// way, so there is nothing left to save.
    fn exit(&self, _held: MutexGuard<'_, State>) -> ! {
        let _ = fs::remove_file(&self.socket);
        std::process::exit(0);
    }
}

/// Queues `frame` for `client`; one that is not keeping up is dropped,
/// which closes its queue and so its connection.
fn deliver(clients: &mut HashMap<u64, Client>, client: u64, frame: Vec<u8>) {
    let Some(to) = clients.get(&client) else {
        return;
    };
    match to.outbox.try_send(frame) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            log::warn!("client {client} stopped reading; disconnecting it");
            if let Some(stuck) = clients.remove(&client) {
                let _ = stuck.socket.shutdown(Shutdown::Both);
            }
        }
        Err(TrySendError::Disconnected(_)) => {
            clients.remove(&client);
        }
    }
}
