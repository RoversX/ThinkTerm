//! The connected clients, and the plugins they call.
//!
//! Each client has a thread reading its calls and one writing what it is
//! sent, from a queue of its own: a client that stops reading loses its
//! connection instead of holding up everyone else's events.
//!
//! A call to a built-in plugin is answered on the caller's thread with the
//! host locked. One to an installed plugin is handed to its program, and
//! answered when the program answers, on the thread that reads it: a slow
//! plugin holds up its own callers and nobody else. Calls addressed to
//! [`api::PLUGIN`] are the host's own: the list of plugins, their switches,
//! and reloading them.

use crate::process::{Listener, Waiter};
use crate::registry::{Fallout, Lookup, Message, Registry};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::Shutdown;
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};
use thinkterm_plugin_channel::registry as api;
use thinkterm_plugin_channel::wire::{read_frame, write_frame, FromHost, ToHost, PROTOCOL};
use thinkterm_plugin_sdk::protocol::{FromPlugin, ToPlugin};
use thinkterm_plugin_sdk::Cx;
use wezterm_uds::UnixStream;

/// Frames that may wait for one client. A client this far behind is not
/// reading, and is disconnected.
const QUEUE: usize = 256;
/// The threads here parse JSON and move bytes; a host with two clients has
/// no use for megabytes of stack each.
const STACK: usize = 256 * 1024;

struct Client {
    outbox: SyncSender<Vec<u8>>,
    /// Shut down to drop a client whose writer may be stuck in a write.
    socket: UnixStream,
    /// The plugins whose events it is sent; [`api::PLUGIN`] for the list's.
    watching: HashSet<String>,
}

struct State {
    registry: Registry,
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
    /// Itself, for what it starts from a program's threads to report to.
    me: Weak<Host>,
}

/// What handling something leaves to send, in the order it goes out: the
/// answers, then the plugins' events, then what the list's watchers hear.
#[derive(Default)]
struct Out {
    answers: Vec<(u64, Vec<u8>)>,
    /// Clients that start watching a plugin, before its events go out.
    watch: Vec<(u64, String)>,
    events: Vec<(String, Value)>,
    changed: bool,
    /// Not told of the change: it asked for the list, and has it.
    fresh: Option<u64>,
}

impl Out {
    fn answer(&mut self, client: u64, id: u64, answer: Result<Value, String>) {
        let frame = match answer {
            Ok(body) => FromHost::Ok { id, body },
            Err(message) => FromHost::Error { id, message },
        };
        self.answers.push((client, frame.encode()));
    }

    fn fallout(&mut self, fallout: Fallout) {
        for (waiter, why) in fallout.unanswered {
            self.answer(waiter.client, waiter.id, Err(why));
        }
        self.changed |= fallout.changed;
    }
}

impl Host {
    pub fn new(socket: PathBuf, idle: Duration, registry: Registry) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            state: Mutex::new(State {
                registry,
                clients: HashMap::new(),
                next_client: 1,
                idle_since: Some(Instant::now()),
            }),
            socket,
            idle,
            me: me.clone(),
        })
    }

    /// Itself as what a program's threads report to.
    fn listener(&self) -> Option<Arc<dyn Listener>> {
        let me = self.me.upgrade()?;
        Some(me)
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

    fn serve(self: &Arc<Self>, client: u64, mut stream: UnixStream) {
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

    fn call(self: &Arc<Self>, client: u64, id: u64, plugin: &str, body: Value) {
        let mut state = self.state();
        let mut out = Out::default();
        if plugin == api::PLUGIN {
            self.manage(&mut state, client, id, body, &mut out);
        } else {
            self.ask(&mut state, client, id, plugin, body, &mut out);
        }
        send(&mut state, out);
    }

    /// A call to the host itself.
    fn manage(
        self: &Arc<Self>,
        state: &mut State,
        client: u64,
        id: u64,
        body: Value,
        out: &mut Out,
    ) {
        let request = match serde_json::from_value::<api::Request>(body) {
            Ok(request) => request,
            Err(err) => {
                let why = format!("not a request the plugin host knows: {err}");
                return out.answer(client, id, Err(why));
            }
        };
        let mut fallout = Fallout::default();
        let answer = match request {
            api::Request::List { locale } => {
                state.registry.scan(&mut fallout);
                out.watch.push((client, api::PLUGIN.to_string()));
                out.fresh = Some(client);
                Ok(serde_json::to_value(state.registry.list(&locale))
                    .expect("a plugin list always serialises"))
            }
            api::Request::SetEnabled {
                id: plugin,
                enabled,
            } => state
                .registry
                .set_enabled(&plugin, enabled, &mut fallout)
                .map(|()| Value::Null),
            api::Request::Reload { id: plugin } => state
                .registry
                .reload(plugin.as_deref(), &mut fallout)
                .map(|()| Value::Null),
        };
        out.answer(client, id, answer);
        out.fallout(fallout);
    }

    /// A call for the plugin `plugin`.
    fn ask(
        self: &Arc<Self>,
        state: &mut State,
        client: u64,
        id: u64,
        plugin: &str,
        body: Value,
        out: &mut Out,
    ) {
        let mut fallout = Fallout::default();
        let found = state.registry.find(plugin, &mut fallout);
        out.fallout(fallout);
        match found {
            Lookup::Missing => out.answer(
                client,
                id,
                Err(format!("there is no plugin named {plugin:?}")),
            ),
            Lookup::Builtin(index) => {
                let enabled = state.registry.enabled(plugin);
                let builtin = state.registry.builtin(index);
                if !enabled {
                    let why = format!("{} is turned off", builtin.manifest.name);
                    return out.answer(client, id, Err(why));
                }
                let mut cx = Cx::new();
                let answer = builtin.plugin.call(body, &mut cx);
                let effects = cx.finish();
                if effects.watch && answer.is_ok() {
                    out.watch.push((client, plugin.to_string()));
                }
                out.answer(client, id, answer.map_err(|err| format!("{err:#}")));
                out.events.extend(
                    effects
                        .events
                        .into_iter()
                        .map(|event| (plugin.to_string(), event)),
                );
            }
            Lookup::Installed(index) => {
                let listener: Arc<dyn Listener> = Arc::clone(self) as Arc<dyn Listener>;
                let mut fallout = Fallout::default();
                let message: Message = Box::new(move |asked| ToPlugin::Call { id: asked, body });
                let asked = state.registry.ask(
                    plugin,
                    index,
                    Waiter { client, id },
                    message,
                    &listener,
                    &mut fallout,
                );
                if let Err(why) = asked {
                    out.answer(client, id, Err(why));
                }
                out.fallout(fallout);
            }
        }
    }

    fn leave(&self, client: u64) {
        let mut state = self.state();
        state.clients.remove(&client);
        // Nobody is left to answer: its calls free the plugins' slots.
        state.registry.forget_client(client);
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
    /// than finding a dead one, stops the plugins' programs and exits.
    /// Every change is on disk by the time it is answered, and holding the
    /// table means no call is under way, so there is nothing left to save.
    fn exit(&self, mut held: MutexGuard<'_, State>) -> ! {
        let _ = fs::remove_file(&self.socket);
        held.registry.shut_down();
        std::process::exit(0);
    }
}

impl Listener for Host {
    fn said(&self, plugin: &str, generation: u64, message: FromPlugin) {
        let mut state = self.state();
        let mut out = Out::default();
        let mut fallout = Fallout::default();
        let running = state.registry.process(plugin, generation).is_some();
        match message {
            FromPlugin::Ready { api } => {
                state.registry.ready(plugin, generation, api, &mut fallout)
            }
            FromPlugin::Ok { id, body, watch } => {
                let waiter = state
                    .registry
                    .process(plugin, generation)
                    .and_then(|process| process.answered(id));
                if let Some(waiter) = waiter {
                    if watch {
                        out.watch.push((waiter.client, plugin.to_string()));
                    }
                    out.answer(waiter.client, waiter.id, Ok(body));
                }
            }
            FromPlugin::Error { id, message } => {
                let waiter = state
                    .registry
                    .process(plugin, generation)
                    .and_then(|process| process.answered(id));
                if let Some(waiter) = waiter {
                    out.answer(waiter.client, waiter.id, Err(message));
                }
            }
            FromPlugin::Event { body } if running => out.events.push((plugin.to_string(), body)),
            FromPlugin::Event { .. } => {}
        }
        out.fallout(fallout);
        send(&mut state, out);
    }

    fn ended(&self, plugin: &str, generation: u64, why: String) {
        let Some(listener) = self.listener() else {
            return;
        };
        let mut state = self.state();
        let mut fallout = Fallout::default();
        state
            .registry
            .ended(plugin, generation, why, &listener, &mut fallout);
        let mut out = Out::default();
        out.fallout(fallout);
        send(&mut state, out);
    }

    fn late(&self, plugin: &str, generation: u64) {
        let mut state = self.state();
        let mut fallout = Fallout::default();
        state.registry.late(plugin, generation, &mut fallout);
        let mut out = Out::default();
        out.fallout(fallout);
        send(&mut state, out);
    }
}

/// Sends what `out` holds.
fn send(state: &mut State, out: Out) {
    for (client, plugin) in out.watch {
        if let Some(watcher) = state.clients.get_mut(&client) {
            watcher.watching.insert(plugin);
        }
    }
    for (client, frame) in out.answers {
        deliver(&mut state.clients, client, frame);
    }
    for (plugin, body) in out.events {
        let frame = FromHost::Event {
            plugin: plugin.clone(),
            body,
        }
        .encode();
        tell_watchers(&mut state.clients, &plugin, None, frame);
    }
    if out.changed {
        let frame = registry_event(&api::Event::Changed);
        tell_watchers(&mut state.clients, api::PLUGIN, out.fresh, frame);
    }
    if state.clients.is_empty() {
        state.idle_since.get_or_insert_with(Instant::now);
    }
}

fn registry_event(event: &api::Event) -> Vec<u8> {
    FromHost::Event {
        plugin: api::PLUGIN.to_string(),
        body: serde_json::to_value(event).expect("an event always serialises"),
    }
    .encode()
}

/// Queues `frame` for every client watching `plugin` but `except`.
fn tell_watchers(
    clients: &mut HashMap<u64, Client>,
    plugin: &str,
    except: Option<u64>,
    frame: Vec<u8>,
) {
    let watchers: Vec<u64> = clients
        .iter()
        .filter(|(id, watcher)| Some(**id) != except && watcher.watching.contains(plugin))
        .map(|(id, _)| *id)
        .collect();
    for watcher in watchers {
        deliver(clients, watcher, frame.clone());
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
