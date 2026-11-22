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
//!
//! A panel a client shows is a view here, numbered for the plugin, routed
//! to the client that opened it and closed when either side goes. Its
//! frames go one at a time: while the client has not taken in the last,
//! only the newest is kept back. A panel's extended view is a view of its
//! own, linked to the panel's, and closed before it whenever it closes.
//! What a plugin asks of the other machine a panel's terminal runs on goes
//! to that panel's client, and the answer back; a panel that closes first
//! leaves its plugin told that none is coming.
//!
//! Whether a plugin is used -- a panel on show, a client watching it, a
//! call waiting -- is looked at after everything handled here, and a timer
//! stops the programs gone unused for long enough ([`Registry::tidy`]).
//! Clients that are ThinkTerm running on this machine say so (`keep`):
//! while one is connected, the plugins that run always do.

use crate::process::{Listener, Waiter, STACK};
use crate::registry::{Fallout, Lookup, Message, Refusal, Registry};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::Shutdown;
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};
use thinkterm_plugin_channel::registry as api;
use thinkterm_plugin_channel::wire::{
    frame_header, read_frame, write_frame, FromHost, PageArgs, PanelEvent, PanelRequest, Raw,
    ToHost, PROTOCOL,
};
use thinkterm_plugin_sdk::panel::{feature, Answer, Env, Input, Picture, RowsWanted};
use thinkterm_plugin_sdk::protocol::{FromPlugin, ToPlugin};
use thinkterm_plugin_sdk::Cx;
use wezterm_uds::UnixStream;

/// Frames that may wait for one client. A client this far behind is not
/// reading, and is disconnected.
const QUEUE: usize = 256;
/// What one panel's plugin may have asked of its client and not heard back
/// about. A plugin this far ahead is not waiting for its answers.
const ASKS: usize = 64;
/// How often the views the host could not open are looked at again, while
/// any wait: whether their plugins can run now.
const REFUSED_EVERY: Duration = Duration::from_secs(1);
/// How often the running programs' files are looked at, and how long a
/// change to them has to stay as it is before the program is started again:
/// an editor, or an agent, writes several files one after another.
const FILES_EVERY: Duration = Duration::from_millis(500);
const FILES_SETTLE: Duration = Duration::from_millis(400);
/// How long a client has to draw a picture of a view it was asked for.
const PICTURE_WITHIN: Duration = Duration::from_secs(10);
/// The views one client may have waiting for their plugins; past this, its
/// oldest is forgotten.
const REFUSED_PER_CLIENT: usize = 32;
/// The pictures of views the host keeps, newest; older ones are let go.
const PICTURES_KEPT: usize = 20;

struct Client {
    outbox: SyncSender<Vec<u8>>,
    /// Shut down to drop a client whose writer may be stuck in a write.
    socket: UnixStream,
    /// The plugins whose events it is sent; [`api::PLUGIN`] for the list's.
    watching: HashSet<String>,
}

/// A panel a client shows, by the number its plugin knows it by.
struct Route {
    client: u64,
    /// The client's own number for it.
    view: u64,
    plugin: String,
    /// The plugin's run it is open on.
    generation: u64,
    /// For an extended view, the number of the panel it extends.
    extends: Option<u64>,
    /// A frame is with the client, and it has not said it took it in.
    in_flight: bool,
    /// The newest frame drawn meanwhile.
    held: Option<Raw>,
    /// Pages of rows asked for and not answered: rows beyond these are
    /// not the client's to be sent.
    asked: u32,
    /// The client said the panel's terminal runs on another machine
    /// (`Env::remote`), which the plugin may ask things of.
    remote: bool,
    /// What the plugin asked of it that the client has not answered.
    asks: HashSet<u64>,
    /// It is a page of the plugin's, not its panel.
    page: bool,
    /// The client draws it into a picture when asked (`feature::PICTURE`).
    pictures: bool,
}

struct State {
    registry: Registry,
    clients: HashMap<u64, Client>,
    next_client: u64,
    views: HashMap<u64, Route>,
    next_view: u64,
    /// When the last client left, or the host started; `None` while anyone
    /// is connected.
    idle_since: Option<Instant>,
    /// The clients that are ThinkTerm running on this machine.
    keepers: HashSet<u64>,
    /// When a program is next due to be stopped or started.
    next_tidy: Option<Instant>,
    /// The views the host could not open because their plugin could not
    /// run -- turned off, failed -- by client and the client's own number,
    /// with the plugin: told to open again once it can, which a client
    /// still showing one does. A client letting go of one forgets it.
    refused: Vec<Refused>,
    /// When `refused` was last looked at.
    refused_looked: Option<Instant>,
    /// Pictures of views asked of the clients showing them, by the number
    /// they were asked under.
    pictures: HashMap<u64, Asked>,
    next_picture: u64,
}

/// A picture of a view asked of the client showing it, for whoever called.
struct Asked {
    /// Who called, and their id for the call.
    caller: u64,
    call: u64,
    /// The view, by the host's number for it, and whose it is.
    view: u64,
    plugin: String,
    page: bool,
    until: Instant,
}

/// A view the host could not open, waiting for its plugin.
struct Refused {
    client: u64,
    view: u64,
    plugin: String,
}

pub struct Host {
    state: Mutex<State>,
    /// Wakes the timer when what it waits for changed.
    wake: Condvar,
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
    /// About panels, each for the client showing it.
    panels: Vec<(u64, Vec<u8>)>,
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

    /// False when `event` is too long for a frame: a plugin's line as long
    /// as it may be is, with the view's number around it. It is dropped,
    /// since sending it would cost the client its connection.
    fn panel(&mut self, client: u64, view: u64, event: PanelEvent) -> bool {
        let frame = FromHost::Panel { view, event }.encode();
        if frame_header(frame.len()).is_none() {
            log::warn!(
                "dropped {} bytes drawn for a panel of client {client}: too long to send",
                frame.len()
            );
            return false;
        }
        self.panels.push((client, frame));
        true
    }

    fn closed(&mut self, client: u64, view: u64, reason: String, again: bool) {
        self.panel(client, view, PanelEvent::Closed { reason, again });
    }
}

impl Host {
    pub fn new(socket: PathBuf, idle: Duration, registry: Registry) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            state: Mutex::new(State {
                registry,
                clients: HashMap::new(),
                next_client: 1,
                views: HashMap::new(),
                next_view: 0,
                idle_since: Some(Instant::now()),
                keepers: HashSet::new(),
                next_tidy: None,
                refused: Vec::new(),
                refused_looked: None,
                pictures: HashMap::new(),
                next_picture: 0,
            }),
            wake: Condvar::new(),
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
                Ok(ToHost::Panel { view, request }) => self.panel(client, view, request),
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
        self.send(&mut state, out);
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
        if let api::Request::Picture { id: plugin, page } = &request {
            // Answered once the client showing the view has drawn it.
            if let Err(why) = self.ask_picture(state, client, id, plugin, *page, out) {
                out.answer(client, id, Err(why));
            }
            return;
        }
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
            api::Request::SetBackground {
                id: plugin,
                background,
            } => state
                .registry
                .set_background(&plugin, background, &mut fallout)
                .map(|()| Value::Null),
            api::Request::Keep => {
                // A keeper asks again when a plugin is installed that says
                // it runs always: looked for, it is marked and started.
                if state.keepers.insert(client) {
                    log::info!("client {client} is ThinkTerm running on this machine");
                }
                state.registry.scan(&mut fallout);
                Ok(Value::Null)
            }
            api::Request::Reload { id: plugin } => state
                .registry
                .reload(plugin.as_deref(), &mut fallout)
                .map(|()| Value::Null),
            api::Request::Allow { id: plugin, dir } => state
                .registry
                .allow(&plugin, &dir, &mut fallout)
                .map(|()| Value::Null),
            api::Request::Logs { id: plugin } => {
                state.registry.scan(&mut fallout);
                state.registry.logs(&plugin).map(|logs| {
                    serde_json::to_value(logs).expect("a plugin's logs always serialise")
                })
            }
            api::Request::Picture { .. } => unreachable!("asked of a client above"),
        };
        out.answer(client, id, answer);
        // A plugin turned on or reloaded may serve the views it could not.
        state.refused_looked = None;
        out.fallout(fallout);
    }

    /// Asks the client showing plugin `plugin`'s view -- its panel, or a
    /// page, as `page` says, else the one view of it that is open -- to
    /// draw it into a picture, for `caller`'s call `call`. Why not, when it
    /// cannot be asked.
    #[allow(clippy::too_many_arguments)]
    fn ask_picture(
        &self,
        state: &mut State,
        caller: u64,
        call: u64,
        plugin: &str,
        page: Option<bool>,
        out: &mut Out,
    ) -> Result<(), String> {
        let open: Vec<(u64, &Route)> = state
            .views
            .iter()
            .filter(|(_, route)| route.plugin == plugin && route.extends.is_none())
            .map(|(number, route)| (*number, route))
            .collect();
        if open.is_empty() {
            return Err(format!(
                "no view of {plugin:?} is open: open its page -- `thinkterm plugin do {plugin} open`, \
                 when it has an action that does -- or its panel, in the right sidebar, first"
            ));
        }
        let asked: Vec<(u64, &Route)> = open
            .iter()
            .copied()
            .filter(|(_, route)| page.is_none_or(|page| route.page == page))
            .collect();
        if asked.is_empty() {
            return Err(match page {
                Some(true) => format!("no page of {plugin:?} is open"),
                _ => format!("the panel of {plugin:?} is not open"),
            });
        }
        let drawable: Vec<(u64, &Route)> = asked
            .iter()
            .copied()
            .filter(|(_, route)| route.pictures)
            .collect();
        if drawable.is_empty() {
            return Err(format!(
                "{plugin:?} is open only where its view cannot be drawn into a picture: \
                 a browser, or a ThinkTerm older than this one"
            ));
        }
        let panels = drawable.iter().filter(|(_, route)| !route.page).count();
        let pages = drawable.len() - panels;
        if page.is_none() && drawable.len() > 1 {
            return Err(format!(
                "{plugin:?} is open in {} places -- {} -- so say which: --page or --panel",
                drawable.len(),
                match (panels, pages) {
                    (0, pages) => format!("{pages} pages"),
                    (panels, 0) => format!("{panels} panels"),
                    (1, 1) => "its panel and a page".to_string(),
                    (1, pages) => format!("its panel and {pages} pages"),
                    (panels, pages) => format!("{panels} panels and {pages} pages"),
                }
            ));
        }
        // Of several asked for by kind, the one opened last.
        let (number, route) = drawable
            .iter()
            .max_by_key(|(number, _)| *number)
            .copied()
            .expect("one at least");
        let (client, view, is_page) = (route.client, route.view, route.page);
        state.next_picture += 1;
        let picture = state.next_picture;
        if !out.panel(client, view, PanelEvent::Picture { id: picture }) {
            return Err("the view cannot be asked for a picture".into());
        }
        state.pictures.insert(
            picture,
            Asked {
                caller,
                call,
                view: number,
                plugin: plugin.to_string(),
                page: is_page,
                until: Instant::now() + PICTURE_WITHIN,
            },
        );
        Ok(())
    }

    /// A picture the client drew of its view, as asked under `id`: kept,
    /// and where it was put told to whoever asked.
    fn picture_drawn(
        &self,
        state: &mut State,
        client: u64,
        view: u64,
        id: u64,
        picture: Raw,
        out: &mut Out,
    ) {
        let Some(asked) = state.pictures.get(&id) else {
            // Given up on already.
            return;
        };
        let by = state
            .views
            .get(&asked.view)
            .map(|route| (route.client, route.view));
        if by != Some((client, view)) {
            log::warn!("client {client} sent a picture of a view it was not asked for");
            return;
        }
        let asked = state.pictures.remove(&id).expect("found above");
        let answer = match picture.read::<Picture>() {
            Ok(Picture::Drawn { png, width, height }) => {
                self.keep_picture(&asked.plugin, id, &png.0).map(|path| {
                    let kept = api::Picture {
                        path: path.display().to_string(),
                        width,
                        height,
                        page: asked.page,
                    };
                    serde_json::to_value(kept).expect("a picture always serialises")
                })
            }
            Ok(Picture::Failed { why }) => Err(format!("ThinkTerm could not draw it: {why}")),
            Err(err) => Err(format!("what ThinkTerm drew does not read: {err}")),
        };
        out.answer(asked.caller, asked.call, answer);
    }

    /// Writes `png`, a picture of plugin `plugin`'s view asked for as
    /// `number`, into the host's own directory of them, beside its socket,
    /// readable by the user alone; the oldest past [`PICTURES_KEPT`] go.
    fn keep_picture(&self, plugin: &str, number: u64, png: &[u8]) -> Result<PathBuf, String> {
        let dir = self
            .socket
            .parent()
            .map(|dir| dir.join("plugin-pictures"))
            .ok_or_else(|| "the host has no directory to keep it in".to_string())?;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&dir)
            .map_err(|err| format!("cannot make {}: {err}", dir.display()))?;
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!("{plugin}-{millis}-{number}.png"));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let written = options.open(&path).and_then(|mut file| {
            use std::io::Write;
            file.write_all(png)
        });
        written.map_err(|err| format!("cannot write {}: {err}", path.display()))?;
        // The newest kept: the names sort by when, within a plugin's.
        if let Ok(entries) = fs::read_dir(&dir) {
            let mut kept: Vec<(std::time::SystemTime, PathBuf)> = entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "png"))
                .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
                .collect();
            kept.sort();
            let extra = kept.len().saturating_sub(PICTURES_KEPT);
            for (_, old) in kept.into_iter().take(extra) {
                let _ = fs::remove_file(old);
            }
        }
        Ok(path)
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

    /// Something about the client's panel `view`.
    fn panel(self: &Arc<Self>, client: u64, view: u64, request: PanelRequest) {
        let mut state = self.state();
        let mut out = Out::default();
        let mut fallout = Fallout::default();
        if let PanelRequest::Open {
            plugin,
            env,
            extends,
            page,
        } = request
        {
            self.open_panel(&mut state, client, view, plugin, env, extends, page, &mut out);
            return self.send(&mut state, out);
        }
        if let PanelRequest::Picture { id, picture } = request {
            self.picture_drawn(&mut state, client, view, id, picture, &mut out);
            return self.send(&mut state, out);
        }
        let Some(number) = route_of(&state.views, client, view) else {
            // Closed already: the client hears so, or has. One it lets go
            // of is not opened again.
            if matches!(request, PanelRequest::Close) {
                state
                    .refused
                    .retain(|refused| !(refused.client == client && refused.view == view));
            }
            return;
        };
        // As a plugin's drawing is, in `said`.
        let drawing = matches!(request, PanelRequest::Shown | PanelRequest::Rows { .. });
        let route = state.views.get_mut(&number).expect("just found");
        let told = match request {
            PanelRequest::Open { .. } => unreachable!("handled above"),
            PanelRequest::Env { env } => match env.read::<Env>() {
                Ok(env) => {
                    route.remote = env.remote.is_some();
                    route.pictures = env.has(feature::PICTURE);
                    Some(Told::Message(ToPlugin::Env { view: number, env }))
                }
                Err(err) => {
                    log::warn!("client {client} sent a panel size that does not read: {err}");
                    None
                }
            },
            PanelRequest::Input { input } => match input.read::<Input>() {
                Ok(input) => {
                    // The client shows a press as pressed until it hears.
                    if matches!(input, Input::Click(_)) {
                        out.panel(client, view, PanelEvent::Heard);
                    }
                    Some(Told::Message(ToPlugin::Input {
                        view: number,
                        input,
                    }))
                }
                Err(err) => {
                    log::warn!("client {client} sent input that does not read: {err}");
                    None
                }
            },
            PanelRequest::Rows { wanted } => match wanted.read::<RowsWanted>() {
                Ok(wanted) => {
                    route.asked += 1;
                    Some(Told::Message(ToPlugin::Rows {
                        view: number,
                        wanted,
                    }))
                }
                Err(err) => {
                    log::warn!("client {client} asked for rows in a way that does not read: {err}");
                    None
                }
            },
            PanelRequest::Shown => {
                route.in_flight = match route.held.take() {
                    Some(frame) => out.panel(client, view, PanelEvent::Frame { frame }),
                    None => false,
                };
                None
            }
            PanelRequest::Close => {
                self.close_view(&mut state, number, &mut fallout, &mut out);
                None
            }
            // One the plugin stopped waiting for, or never asked, goes no
            // further.
            PanelRequest::Answer { id, answer } => route
                .asks
                .remove(&id)
                .then(|| Told::Line(answer_line(number, id, &answer))),
            PanelRequest::Picture { .. } => unreachable!("handled above"),
        };
        if let Some(told) = told {
            let route = &state.views[&number];
            let (plugin, generation) = (route.plugin.clone(), route.generation);
            match told {
                Told::Message(message) => {
                    state
                        .registry
                        .tell(&plugin, generation, &message, &mut fallout);
                }
                Told::Line(line) => {
                    state
                        .registry
                        .tell_line(&plugin, generation, line, &mut fallout);
                }
            }
        }
        out.fallout(fallout);
        if drawing {
            deliver_out(&mut state, out);
        } else {
            self.send(&mut state, out);
        }
    }

    /// Opens plugin `plugin`'s panel as the client's `view`, replacing any
    /// it had under that number: the extended view of the client's panel
    /// `extends`, with one, and a page of the plugin's with `page`.
    #[allow(clippy::too_many_arguments)]
    fn open_panel(
        self: &Arc<Self>,
        state: &mut State,
        client: u64,
        view: u64,
        plugin: String,
        env: Raw,
        extends: Option<u64>,
        page: Option<PageArgs>,
        out: &mut Out,
    ) {
        let mut fallout = Fallout::default();
        if let Some(number) = route_of(&state.views, client, view) {
            self.close_view(state, number, &mut fallout, out);
        }
        let env = match env.read::<Env>() {
            Ok(env) => env,
            Err(err) => {
                out.fallout(fallout);
                return out.closed(client, view, format!("not a panel's size: {err}"), false);
            }
        };
        let remote = env.remote.is_some();
        let pictures = env.has(feature::PICTURE);
        // A page stands by itself: it is nothing's extended view.
        if page.is_some() && extends.is_some() {
            out.fallout(fallout);
            return out.closed(client, view, "a page extends no panel".into(), false);
        }
        // What an extended view extends is a panel of the same plugin that
        // the client shows, and not an extended view itself.
        let extends = match extends {
            None => None,
            Some(panel) => {
                let found = route_of(&state.views, client, panel).filter(|number| {
                    let route = &state.views[number];
                    route.plugin == plugin && route.extends.is_none()
                });
                let Some(found) = found else {
                    out.fallout(fallout);
                    let why = format!("the client shows no panel {panel} of {plugin} to extend");
                    return out.closed(client, view, why, false);
                };
                // A panel has one extended view: a new one replaces it.
                self.close_extension(state, found, "another took its place", &mut fallout, out);
                Some(found)
            }
        };
        let index = match state.registry.find(&plugin, &mut fallout) {
            Lookup::Installed(index) => Some(index),
            Lookup::Builtin(_) | Lookup::Missing => None,
        };
        state.next_view += 1;
        let number = state.next_view;
        let opened = match index {
            Some(index) => {
                let listener: Arc<dyn Listener> = Arc::clone(self) as Arc<dyn Listener>;
                let message = ToPlugin::Open {
                    view: number,
                    env,
                    extends,
                    page: page.clone(),
                };
                state
                    .registry
                    .open_panel(&plugin, index, &message, &listener, &mut fallout)
            }
            None => Err(Refusal {
                reason: match page {
                    Some(_) => format!("there is no plugin named {plugin:?} with a page"),
                    None => format!("there is no plugin named {plugin:?} with a panel"),
                },
                again: false,
            }),
        };
        out.fallout(fallout);
        state
            .refused
            .retain(|refused| !(refused.client == client && refused.view == view));
        match opened {
            Ok(generation) => {
                let route = Route {
                    client,
                    view,
                    plugin,
                    generation,
                    extends,
                    in_flight: false,
                    held: None,
                    asked: 0,
                    remote,
                    asks: HashSet::new(),
                    page: page.is_some(),
                    pictures,
                };
                state.views.insert(number, route);
            }
            Err(Refusal { reason, again }) => {
                // One that cannot run now may later: once it can, the
                // client is told to try again. One that can run, and still
                // would not open -- no panel, say -- is not.
                if !again && index.is_some() && !state.registry.servable(&plugin) {
                    let held = state
                        .refused
                        .iter()
                        .filter(|refused| refused.client == client)
                        .count();
                    if held >= REFUSED_PER_CLIENT {
                        if let Some(oldest) = state
                            .refused
                            .iter()
                            .position(|refused| refused.client == client)
                        {
                            state.refused.remove(oldest);
                        }
                    }
                    state.refused.push(Refused {
                        client,
                        view,
                        plugin: plugin.clone(),
                    });
                }
                out.closed(client, view, reason, again)
            }
        }
    }

    /// Lets go of the view numbered `number`, telling its plugin, and of
    /// its extended view before it, telling the client as well.
    fn close_view(&self, state: &mut State, number: u64, fallout: &mut Fallout, out: &mut Out) {
        self.close_extension(state, number, "its panel closed", fallout, out);
        if let Some(route) = state.views.remove(&number) {
            unanswered(state, number, &route, fallout);
            let message = ToPlugin::Close { view: number };
            state
                .registry
                .tell(&route.plugin, route.generation, &message, fallout);
        }
    }

    /// Lets go of the extended view of the panel numbered `panel`, if it has
    /// one, telling its plugin and its client why.
    fn close_extension(
        &self,
        state: &mut State,
        panel: u64,
        why: &str,
        fallout: &mut Fallout,
        out: &mut Out,
    ) {
        let extensions: Vec<u64> = state
            .views
            .iter()
            .filter(|(_, route)| route.extends == Some(panel))
            .map(|(extension, _)| *extension)
            .collect();
        for extension in extensions {
            if let Some(route) = state.views.remove(&extension) {
                unanswered(state, extension, &route, fallout);
                let message = ToPlugin::Close { view: extension };
                state
                    .registry
                    .tell(&route.plugin, route.generation, &message, fallout);
                out.closed(route.client, route.view, why.into(), false);
            }
        }
    }

    fn leave(&self, client: u64) {
        let mut state = self.state();
        state.clients.remove(&client);
        state.keepers.remove(&client);
        state.refused.retain(|refused| refused.client != client);
        // Nobody is left to answer: its calls free the plugins' slots.
        state.registry.forget_client(client);
        let views: Vec<u64> = state
            .views
            .iter()
            .filter(|(_, route)| route.client == client)
            .map(|(number, _)| *number)
            .collect();
        let mut fallout = Fallout::default();
        let mut out = Out::default();
        for number in views {
            self.close_view(&mut state, number, &mut fallout, &mut out);
        }
        out.fallout(fallout);
        self.send(&mut state, out);
        if state.clients.is_empty() {
            state.idle_since.get_or_insert_with(Instant::now);
        }
    }

    /// Stops the programs gone unused as they come due, and exits once
    /// nobody has been connected for the idle time.
    pub fn exit_when_idle(self: &Arc<Self>) {
        let host = Arc::clone(self);
        let reaper = std::thread::Builder::new()
            .name("idle".into())
            .stack_size(STACK)
            .spawn(move || {
                // Decided with the table locked, so a client being accepted
                // either registers first or finds the socket gone and starts
                // a new host.
                let mut state = host.state();
                loop {
                    let now = Instant::now();
                    let idle_left = match state.idle_since {
                        Some(since) if now.saturating_duration_since(since) >= host.idle => {
                            log::info!("nobody connected for {:?}; exiting", host.idle);
                            host.exit(state);
                        }
                        Some(since) => host.idle - now.saturating_duration_since(since),
                        None => host.idle,
                    };
                    if state.next_tidy.is_some_and(|due| due <= now) {
                        host.send(&mut state, Out::default());
                    }
                    let wait = state.next_tidy.map_or(idle_left, |due| {
                        idle_left.min(due.saturating_duration_since(Instant::now()))
                    });
                    state = match host.wake.wait_timeout(state, wait) {
                        Ok((state, _)) => state,
                        Err(poisoned) => poisoned.into_inner().0,
                    };
                }
            });
        if let Err(err) = reaper {
            log::warn!("cannot watch for idleness, so this host stays up: {err}");
        }
    }

    /// Has the host told each time Python has been looked for: the calls
    /// that waited for it go on, and the views that did open.
    pub fn follow_python(self: &Arc<Self>) {
        let host = Arc::downgrade(self);
        crate::python::when_looked(move || {
            if let Some(host) = host.upgrade() {
                host.python_looked();
            }
        });
    }

    fn python_looked(&self) {
        let Some(listener) = self.listener() else {
            return;
        };
        let mut state = self.state();
        let mut fallout = Fallout::default();
        state.registry.send_queued(&listener, &mut fallout);
        state.refused_looked = None;
        let mut out = Out::default();
        out.fallout(fallout);
        self.send(&mut state, out);
    }

    /// Starts the programs running again whose files changed -- a script
    /// edited, a program rebuilt -- once the change has settled, so their
    /// views show what they are now without anyone asking. The files are
    /// looked at without the host locked.
    pub fn watch_files(self: &Arc<Self>) {
        let host = Arc::clone(self);
        let watching = std::thread::Builder::new()
            .name("plugin-files".into())
            .stack_size(STACK)
            .spawn(move || {
                // A change seen, by run: what the files came to, and since when.
                let mut settling: HashMap<u64, (Option<crate::stamp::Stamp>, Instant)> =
                    HashMap::new();
                loop {
                    std::thread::sleep(FILES_EVERY);
                    let running = host.state().registry.running_sources();
                    let mut changed = Vec::new();
                    settling.retain(|generation, _| {
                        running.iter().any(|(_, run, _, _)| run == generation)
                    });
                    for (id, generation, source, then) in running {
                        let now = source.stamp();
                        if now == then {
                            settling.remove(&generation);
                            continue;
                        }
                        match settling.get(&generation) {
                            Some((seen, since)) if *seen == now => {
                                if since.elapsed() >= FILES_SETTLE {
                                    settling.remove(&generation);
                                    changed.push((id, generation));
                                }
                            }
                            _ => {
                                settling.insert(generation, (now, Instant::now()));
                            }
                        }
                    }
                    if changed.is_empty() {
                        continue;
                    }
                    let mut state = host.state();
                    let mut fallout = Fallout::default();
                    for (id, generation) in changed {
                        log::info!("plugin {id}'s files changed: starting it again");
                        state
                            .registry
                            .restart_changed(&id, generation, &mut fallout);
                    }
                    let mut out = Out::default();
                    out.fallout(fallout);
                    host.send(&mut state, out);
                }
            });
        if let Err(err) = watching {
            log::warn!("cannot watch the plugins' files, so an edit waits for a call: {err}");
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
        // A panel's drawing comes many times a second, and changes nothing
        // about which plugins are used: it is sent on without a tidy.
        let drawing = matches!(message, FromPlugin::Frame { .. } | FromPlugin::Rows { .. });
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
            FromPlugin::Frame { view, frame } => {
                if let Some(route) = routed(&mut state.views, view, plugin, generation) {
                    if route.in_flight {
                        route.held = Some(frame);
                    } else {
                        let (client, view) = (route.client, route.view);
                        route.in_flight = out.panel(client, view, PanelEvent::Frame { frame });
                    }
                }
            }
            FromPlugin::Rows { view, rows } => {
                if let Some(route) = routed(&mut state.views, view, plugin, generation) {
                    if route.asked > 0 {
                        route.asked -= 1;
                        let (client, view) = (route.client, route.view);
                        out.panel(client, view, PanelEvent::Rows { rows });
                    }
                }
            }
            FromPlugin::Ask {
                view,
                id,
                machine,
                ask,
            } => {
                let refused = match routed(&mut state.views, view, plugin, generation) {
                    None => Some("the panel is closed"),
                    Some(route) if !route.remote => {
                        Some("the panel's terminal runs on this machine, which the plugin reaches itself")
                    }
                    Some(route) if route.asks.len() >= ASKS => {
                        Some("it asked too much at once")
                    }
                    Some(route) => {
                        route.asks.insert(id);
                        let (client, client_view) = (route.client, route.view);
                        let event = PanelEvent::Remote { id, machine, ask };
                        if out.panel(client, client_view, event) {
                            None
                        } else {
                            route.asks.remove(&id);
                            Some("what it asked is too long to send")
                        }
                    }
                };
                if let Some(why) = refused {
                    let message = ToPlugin::Answer {
                        view,
                        id,
                        answer: Answer::failed(why),
                    };
                    state
                        .registry
                        .tell(plugin, generation, &message, &mut fallout);
                }
            }
        }
        out.fallout(fallout);
        if drawing {
            deliver_out(&mut state, out);
        } else {
            self.send(&mut state, out);
        }
    }

    fn ended(&self, plugin: &str, generation: u64, why: String) {
        let Some(listener) = self.listener() else {
            return;
        };
        let mut state = self.state();
        let mut fallout = Fallout::default();
        let mut out = Out::default();
        // Its panels close with it; opened again, they start the plugin
        // anew, or hear why it cannot be.
        let ended: Vec<u64> = state
            .views
            .iter()
            .filter(|(_, route)| route.plugin == plugin && route.generation == generation)
            .map(|(number, _)| *number)
            .collect();
        for number in ended {
            if let Some(route) = state.views.remove(&number) {
                out.closed(route.client, route.view, why.clone(), true);
            }
        }
        state
            .registry
            .ended(plugin, generation, why, &listener, &mut fallout);
        out.fallout(fallout);
        self.send(&mut state, out);
    }

    fn late(&self, plugin: &str, generation: u64) {
        let mut state = self.state();
        let mut fallout = Fallout::default();
        state.registry.late(plugin, generation, &mut fallout);
        let mut out = Out::default();
        out.fallout(fallout);
        self.send(&mut state, out);
    }
}

/// What a client said about a panel that goes on to its plugin.
enum Told {
    Message(ToPlugin),
    /// Written out already: an answer, carried as the client wrote it.
    Line(Vec<u8>),
}

/// The line telling a plugin what came of what it asked, `id`, of panel
/// `view`: the client's `Answer`, unread, on one line as a message must be.
fn answer_line(view: u64, id: u64, answer: &Raw) -> Vec<u8> {
    let answer = answer.get();
    let answer = if answer.contains('\n') {
        serde_json::from_str::<Value>(answer)
            .map(|answer| answer.to_string())
            .unwrap_or_else(|_| "null".into())
    } else {
        answer.to_string()
    };
    format!("{{\"type\":\"answer\",\"view\":{view},\"id\":{id},\"answer\":{answer}}}\n")
        .into_bytes()
}

/// Tells the plugin of the panel numbered `number`, `route`, gone, that
/// what it asked of the panel's client will not be answered.
fn unanswered(state: &mut State, number: u64, route: &Route, fallout: &mut Fallout) {
    for id in &route.asks {
        let message = ToPlugin::Answer {
            view: number,
            id: *id,
            answer: Answer::failed("the panel closed"),
        };
        state
            .registry
            .tell(&route.plugin, route.generation, &message, fallout);
    }
}

impl Host {
    /// Sends what `out` holds, once the plugins' programs are stopped or
    /// started as how they are used now says.
    fn send(&self, state: &mut State, mut out: Out) {
        self.tidy(state, &mut out);
        retry_refused(state, &mut out);
        give_up_pictures(state, &mut out);
        deliver_out(state, out);
    }

    /// Stops the programs gone unused for long enough and starts the ones
    /// that run always, and has the timer wait for the next one due.
    fn tidy(&self, state: &mut State, out: &mut Out) {
        let Some(listener) = self.listener() else {
            return;
        };
        let mut fallout = Fallout::default();
        let State {
            registry,
            clients,
            views,
            keepers,
            next_tidy,
            refused,
            pictures,
            ..
        } = state;
        let used = |plugin: &str| {
            views.values().any(|route| route.plugin == plugin)
                || clients
                    .values()
                    .any(|client| client.watching.contains(plugin))
        };
        let now = Instant::now();
        let next = registry.tidy(now, !keepers.is_empty(), used, &listener, &mut fallout);
        // The views waiting for their plugins are looked at again in time.
        let next = match (next, refused.is_empty()) {
            (next, true) => next,
            (None, false) => Some(now + REFUSED_EVERY),
            (Some(next), false) => Some(next.min(now + REFUSED_EVERY)),
        };
        // A picture not drawn in time is given up on in time.
        let next = match (next, pictures.values().map(|asked| asked.until).min()) {
            (next, None) => next,
            (None, until) => until,
            (Some(next), Some(until)) => Some(next.min(until)),
        };
        out.fallout(fallout);
        if next != *next_tidy {
            *next_tidy = next;
            self.wake.notify_all();
        }
    }
}

/// Answers the calls for pictures that will not come: asked too long ago,
/// or of a view gone since.
fn give_up_pictures(state: &mut State, out: &mut Out) {
    if state.pictures.is_empty() {
        return;
    }
    let now = Instant::now();
    let views = &state.views;
    let gone: Vec<u64> = state
        .pictures
        .iter()
        .filter(|(_, asked)| asked.until <= now || !views.contains_key(&asked.view))
        .map(|(id, _)| *id)
        .collect();
    for id in gone {
        let asked = state.pictures.remove(&id).expect("listed above");
        let why = if views.contains_key(&asked.view) {
            format!(
                "ThinkTerm did not draw the view of {:?} within {PICTURE_WITHIN:?}",
                asked.plugin
            )
        } else {
            format!("the view of {:?} closed before it was drawn", asked.plugin)
        };
        out.answer(asked.caller, asked.call, Err(why));
    }
}

/// Tells the clients whose views waited for their plugins, those that can
/// run now, to open them again -- looking at most every [`REFUSED_EVERY`],
/// unless something just changed.
fn retry_refused(state: &mut State, out: &mut Out) {
    if state.refused.is_empty() {
        return;
    }
    let now = Instant::now();
    if state
        .refused_looked
        .is_some_and(|looked| now.saturating_duration_since(looked) < REFUSED_EVERY)
    {
        return;
    }
    state.refused_looked = Some(now);
    let registry = &state.registry;
    // Each plugin looked at once, however many views wait for it.
    let mut can: HashMap<String, bool> = HashMap::new();
    let (ready, waiting): (Vec<Refused>, Vec<Refused>) = std::mem::take(&mut state.refused)
        .into_iter()
        .partition(|refused| {
            *can.entry(refused.plugin.clone())
                .or_insert_with(|| registry.servable(&refused.plugin))
        });
    state.refused = waiting;
    for refused in ready {
        log::info!(
            "plugin {} can run now: client {} opens its view again",
            refused.plugin,
            refused.client
        );
        out.closed(refused.client, refused.view, "it can run now".into(), true);
    }
}

/// Sends what `out` holds.
fn deliver_out(state: &mut State, out: Out) {
    for (client, plugin) in out.watch {
        if let Some(watcher) = state.clients.get_mut(&client) {
            watcher.watching.insert(plugin);
        }
    }
    for (client, frame) in out.answers {
        deliver(&mut state.clients, client, frame);
    }
    for (client, frame) in out.panels {
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

/// The number of the client's panel `view`.
fn route_of(views: &HashMap<u64, Route>, client: u64, view: u64) -> Option<u64> {
    views
        .iter()
        .find(|(_, route)| route.client == client && route.view == view)
        .map(|(number, _)| *number)
}

/// The view numbered `view`, if it is open on this run of `plugin`: what an
/// earlier run drew is not the panel's any more.
fn routed<'a>(
    views: &'a mut HashMap<u64, Route>,
    view: u64,
    plugin: &str,
    generation: u64,
) -> Option<&'a mut Route> {
    views
        .get_mut(&view)
        .filter(|route| route.plugin == plugin && route.generation == generation)
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
