//! The core thread: owns the wgpu device, the surface the shell lends it,
//! the link to the server and the App -- the same App the browser runs,
//! over the phone's [`MobilePlatform`] and [`SshLink`].
//!
//! The shape is the one the plan fixes: device and pipeline outlive any
//! surface; the surface is optional and carries a generation; drawing is a
//! request the shell fulfils from its display callback; content changes
//! only *ask* for a frame. The thread drives a `LocalPool` for the App's
//! futures and fires the App's timers itself. Nothing here waits on the
//! shell.

use crate::icons;
use crate::link::SshLink;
use crate::painter::{GlyphPainter, GlyphSeams};
use crate::platform::MobilePlatform;
use crate::ssh::{Net, SshParams};
use crate::{Notify, Published, ViewSet};
use anyhow::{anyhow, Context, Result};
use futures::executor::LocalPool;
use futures::task::LocalSpawnExt;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thinkterm_font_core::FontShaper as _;
use thinkterm_font_web::{Face, FontSet};
use thinkterm_render::pipeline::GpuTexture;
use thinkterm_web::app::{build_session, grid_for, App, PaneCell, Setup};
use thinkterm_web::attach::{attach, Attached, ProtocolMismatch};
use thinkterm_web::glyphs::GlyphCache;
use thinkterm_web::gpu::Gpu;
use thinkterm_web::host::AppHost;
use thinkterm_web::keymap::{map_key, DomKey};
use thinkterm_web::platform::{Link, Platform, PointerInput, WheelDelta, WheelInput};
use wezterm_term::KeyModifiers;

pub type MobileApp = App<MobilePlatform, SshLink>;

/// The views are made again at most this often: a burst of output is
/// one publication per frame, not one per change.
const VIEWS_EVERY: Duration = Duration::from_millis(16);

pub struct ConnectParams {
    pub ssh: SshParams,
    pub font_paths: Vec<String>,
    pub size_pt: f64,
    pub device_id: String,
}

pub enum Cmd {
    Attach {
        layer: usize,
        width: u32,
        height: u32,
        scale: f64,
        reply: Sender<u64>,
    },
    Resize {
        generation: u64,
        width: u32,
        height: u32,
        scale: f64,
    },
    Detach {
        generation: u64,
        reply: Sender<()>,
    },
    Render,
    Animate(bool),
    Stats {
        reply: Sender<String>,
    },
    Shutdown,
    Connect {
        params: ConnectParams,
        painter: Box<dyn GlyphPainter>,
    },
    Disconnect,
    /// From the network thread; `serial` names the link it belongs to,
    /// so a retiring thread's last words never reach its successor, and
    /// `dial` the link's dial, so neither do they reach that link's next.
    Net { serial: u64, dial: u64, net: Net },
    /// The first dial finished.
    Dialed { serial: u64, outcome: Result<(), String> },
    /// The attach handshake finished.
    Attached { serial: u64, outcome: Result<Attached, String> },
    Key {
        name: String,
        ctrl: bool,
        alt: bool,
        shift: bool,
    },
    Text(String),
    Paste(String),
    Scroll(i32),
    Composing(bool),
    View {
        name: String,
        reply: Sender<String>,
    },
    /// Upload `id` is begun: its path goes into the pane focused now.
    UploadFor(u64),
    /// A file of this device's for the host's uploads folder; how it
    /// goes is kept in `Published::uploads` under `id`.
    Upload {
        id: u64,
        name: String,
        path: String,
    },
    CancelUpload(u64),
    /// `Core::check_connection`.
    CheckConnection,
    /// `Core::forget_host_key`.
    ForgetHostKey,
    /// `Core::forget_jump_host_key`.
    ForgetJumpHostKey,
    /// Upload `id` is done: its path goes into the pane it was started
    /// from, if that one is still on show.
    PasteUpload {
        id: u64,
        text: String,
    },
    ChromeClick {
        action: String,
        pane: Option<u32>,
        tab: Option<u32>,
    },
    SideClick {
        kind: String,
        id: Option<String>,
        flag: Option<bool>,
    },
    SideKey {
        key: String,
        value: String,
    },
    ContextMenu {
        kind: String,
        id: String,
        reply: Sender<String>,
    },
    MenuAction {
        id: String,
        reply: Sender<String>,
    },
    SetSpace(String),
    TakeOver,
    Pointer {
        kind: String,
        x: f64,
        y: f64,
    },
    /// A mouse's, which hovers, has buttons of its own and modifiers held.
    Mouse {
        kind: String,
        x: f64,
        y: f64,
        button: u8,
        held: bool,
        mods: KeyModifiers,
    },
    Wheel {
        x: f64,
        y: f64,
        lines: f64,
    },
    WheelPx {
        x: f64,
        y: f64,
        px: f64,
    },
    StepFont(f64),
    ScreenText {
        reply: Sender<String>,
    },
    SetSelection {
        anchor: (usize, usize),
        head: (usize, usize),
    },
    ClearSelection,
    Preview {
        pane: usize,
        rows: usize,
    },
    ScreenPreview {
        pane: usize,
    },
    SelectedText {
        reply: Sender<Option<String>>,
    },
    SetSetting {
        key: String,
        value: String,
    },
    SetPalette(Option<String>),
    /// `Core::set_fallback_palette`.
    SetFallbackPalette {
        scheme: Option<String>,
        unchosen: Option<String>,
    },
    SetShown(bool),
    /// The shell shows one pane of a tab at a time, or the split whole.
    OnePane(bool),
    /// The shell read `Core::views` for the first time.
    ViewsRead,
    WantScreen(bool),
    /// The shell shows the views though the terminal is off screen: a list
    /// of this host's threads among other hosts'.
    ListViews(bool),
    TextBetween {
        anchor: (usize, usize),
        head: (usize, usize),
        reply: Sender<Option<String>>,
    },
}

/// The surface currently lent, if any.
struct Target {
    generation: u64,
    /// The shell's native layer or window, kept so the GPU state can be
    /// remade on it without a fresh attach.
    layer: usize,
    width: u32,
    height: u32,
    scale: f64,
}

#[derive(Default)]
struct Stats {
    attaches: u64,
    detaches: u64,
    resizes: u64,
    frames: u64,
    frames_requested: u64,
    errors: u64,
    last_error: String,
    last_frame_us: u128,
    generation: u64,
    phase: u64,
    width: u32,
    height: u32,
    scale: f64,
    bytes_in: u64,
    inputs_sent: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Conn {
    Idle,
    Connecting,
    Attaching,
    Ready,
    Disconnected(String),
}

struct State {
    cmd_tx: Sender<Cmd>,
    instance: wgpu::Instance,
    adapter: Option<wgpu::Adapter>,
    /// The GPU state until the App takes it over.
    gpu: Option<Gpu>,
    app: Option<Rc<MobileApp>>,
    /// The App's connection was down when last told (`report_link`).
    link_down: bool,
    target: Option<Target>,
    next_generation: u64,
    /// One per link made; a network event from an older one is dropped.
    link_serial: u64,
    /// A handshake is on the wire: a surface that comes and goes
    /// meanwhile must not start a second one over the same lease.
    attaching: bool,
    animating: bool,
    stats: Stats,
    notify: Arc<dyn Notify>,
    pool: LocalPool,
    platform: Rc<MobilePlatform>,
    link: Option<SshLink>,
    fonts: Option<Rc<FontSet>>,
    glyph_platform: Option<thinkterm_web::raster::Platform>,
    size_pt: f64,
    device_id: String,
    conn: Conn,
    status: String,
    composing: bool,
    /// The pane each upload under way was started from, where its path is
    /// pasted once it lands, and the server it is on (`App::server_id`
    /// then): a pane id is another terminal's once the server has started
    /// anew, which a reconnect of the App's own can meet. An entry goes
    /// when its upload ends.
    upload_panes: std::collections::HashMap<u64, (thinkterm_proto::PaneId, Option<String>)>,
    /// The host each upload begun was begun on (`icons_host` then): one
    /// whose host has changed by the time its file is ready -- edited and
    /// connected to again -- is not sent to the new one.
    upload_hosts: std::collections::HashMap<u64, Option<(String, u16, String)>>,
    /// The tab the App before zoomed for want of room, and the server it
    /// is on: handed to the next App on that server, which puts it back
    /// when it leaves it (`App::one_pane_zoomed`).
    one_pane_zoomed: Option<(String, (thinkterm_proto::TabId, thinkterm_proto::PaneId))>,
    /// The shell's preferences, applied to each App as it is made.
    settings: Vec<(String, String)>,
    /// The chosen scheme's colours as JSON, applied to every App made.
    palette: Option<String>,
    /// The scheme for a host that chose none, and the one such a host's
    /// palette may be besides the built-in (`Cmd::SetFallbackPalette`),
    /// as JSON; applied to every App made.
    fallback: Option<(String, Option<String>)>,
    /// A scheme was chosen while the terminal was off screen or the link
    /// down, when the App tells the server nothing of it: told when it is
    /// shown again over a link that is up (`tell_palette`).
    palette_untold: bool,
    /// Where the views and rows go for the shell to read (`Core::views`).
    published: Arc<Mutex<Published>>,
    /// The shell reads the views there; until it does, none are made.
    views_read: Arc<AtomicBool>,
    /// Something the views show changed since they were last made.
    views_dirty: Rc<Cell<bool>>,
    /// The shell's text input reads the rows: keep them current.
    screen_wanted: bool,
    /// The views are on show without the terminal (`Cmd::ListViews`): they
    /// are made off screen too.
    views_listed: bool,
    /// The tab icon cards on show, shared with the shell's thread, which
    /// draws them: the built-in ones, then the host's (`Net::TabIcons`).
    icons: Arc<icons::Shown>,
    /// The host (address, port, user) whose cards those are.
    icons_host: Option<(String, u16, String)>,
    /// A frame was painted since: the rows may have changed.
    screen_dirty: bool,
    /// The shell just asked for the rows: the next made go out as new.
    screen_fresh: bool,
    views_made: Instant,
}

pub fn run(
    cmd_tx: Sender<Cmd>,
    rx: Receiver<Cmd>,
    notify: Box<dyn Notify>,
    published: Arc<Mutex<Published>>,
    views_read: Arc<AtomicBool>,
    icons: Arc<icons::Shown>,
) {
    let mut state = State::new(cmd_tx, notify, published, views_read, icons);
    state.notify.on_log("core thread up".into());
    state.set_status("idle");

    loop {
        match rx.recv_timeout(state.wait(Instant::now())) {
            Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(cmd) => state.handle(cmd),
            Err(RecvTimeoutError::Timeout) => {
                if state.app.is_none() && state.animating {
                    state.stats.phase = state.stats.phase.wrapping_add(1);
                    state.request_frame();
                }
            }
        }
        state.pool.run_until_stalled();
        state.platform.fire_due();
        state.pool.run_until_stalled();
        state.report_link();
        state.publish_views();
    }
    state.disconnect();
    state.notify.on_log("core thread down".into());
}

impl State {
    fn new(
        cmd_tx: Sender<Cmd>,
        notify: Box<dyn Notify>,
        published: Arc<Mutex<Published>>,
        views_read: Arc<AtomicBool>,
        icons: Arc<icons::Shown>,
    ) -> Self {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            #[cfg(target_vendor = "apple")]
            backends: wgpu::Backends::METAL,
            #[cfg(not(target_vendor = "apple"))]
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let pool = LocalPool::new();
        let notify: Arc<dyn Notify> = Arc::from(notify);
        let platform = Rc::new(MobilePlatform::new(pool.spawner(), Arc::clone(&notify)));
        Self {
            cmd_tx,
            instance,
            adapter: None,
            gpu: None,
            app: None,
            link_down: false,
            target: None,
            next_generation: 1,
            link_serial: 0,
            attaching: false,
            animating: false,
            stats: Stats::default(),
            notify,
            pool,
            platform,
            link: None,
            fonts: None,
            glyph_platform: None,
            size_pt: 11.0,
            device_id: String::new(),
            conn: Conn::Idle,
            status: String::new(),
            composing: false,
            upload_panes: Default::default(),
            upload_hosts: Default::default(),
            one_pane_zoomed: None,
            settings: Vec::new(),
            palette: None,
            fallback: None,
            palette_untold: false,
            published,
            views_read,
            views_dirty: Rc::new(Cell::new(true)),
            screen_wanted: false,
            views_listed: false,
            icons,
            icons_host: None,
            screen_dirty: false,
            screen_fresh: false,
            views_made: Instant::now(),
        }
    }

    /// How long the loop may sleep: until a timer is due, a frame of the
    /// idle drawing, or the views owed, which are owed only on show (off
    /// screen they wait, `publish_views`, and the loop with them).
    fn wait(&self, now: Instant) -> Duration {
        let mut wait = Duration::from_secs(3600);
        if self.animating && self.target.is_some() && self.app.is_none() {
            wait = Duration::from_millis(100);
        }
        if let Some(due) = self.platform.next_deadline() {
            wait = wait.min(due.saturating_duration_since(now));
        }
        let owed = self.views_dirty.get() || (self.screen_dirty && self.screen_wanted);
        if owed && (self.platform.shown() || (self.views_listed && self.views_dirty.get())) {
            wait = wait.min((self.views_made + VIEWS_EVERY).saturating_duration_since(now));
        }
        wait
    }

    /// Hand the shell what changed, at most once a frame: the views after
    /// a change the App reported, the rows after a paint while they are
    /// wanted. Everything is made before the lock, which the shell takes
    /// every frame. Called between commands, never from inside the App,
    /// whose state the views borrow.
    fn publish_views(&mut self) {
        if !self.platform.shown() && !self.views_listed {
            // Nobody looks at a terminal off screen (the app in the back,
            // another host's in front): what changed is made once it is
            // back. A list of its threads is looked at, and kept current.
            return;
        }
        let views_dirty = self.views_dirty.get();
        if !self.screen_wanted || !self.platform.shown() {
            // Rows nobody reads are not made at each paint.
            self.screen_dirty = false;
        }
        if !views_dirty && !self.screen_dirty {
            return;
        }
        let now = Instant::now();
        if now < self.views_made + VIEWS_EVERY {
            return;
        }
        self.views_dirty.set(false);
        self.screen_dirty = false;
        self.views_made = now;
        let views = (views_dirty && self.views_read.load(Ordering::Relaxed)).then(|| self.view_set());
        // A change of view (another pane focused) changes the rows too.
        let rows = (self.screen_wanted && self.platform.shown()).then(|| self.screen_json());
        let fresh = std::mem::take(&mut self.screen_fresh);
        if let Ok(mut published) = self.published.lock() {
            if let Some(mut views) = views {
                views.revision = published.views.revision + 1;
                published.views = views;
            }
            if let Some(rows) = rows {
                if fresh || rows != published.screen {
                    published.screen = rows;
                    published.screen_revision += 1;
                }
            }
        }
        // Rows alone come without it: a shell that reads none (Android)
        // would otherwise pull every view at each paint.
        if views_dirty {
            self.notify.on_change();
        }
    }

    /// The App's connection went down -- redialing by itself, or left for
    /// the user -- or came back: the shell hears at once, on screen or off
    /// (`Notify::on_published`, "link"), where the views wait for the
    /// screen. What keeps a phone's connections up while the app is away
    /// goes by it (Android's `ConnectionService`).
    fn report_link(&mut self) {
        let down = self.app.as_ref().is_some_and(|app| !app.connected());
        if down != self.link_down {
            self.link_down = down;
            let said = if down { "down" } else { "up" };
            self.notify.on_published("link".into(), said.into());
        }
    }

    fn view_set(&self) -> ViewSet {
        ViewSet {
            revision: 0,
            connection: self.link_serial,
            tabs: self.view("tabs"),
            threads: self.view("threads"),
            tree: self.view("tree"),
            sidebar: self.view("sidebar"),
            navs: self.view("navs"),
            panes: self.view("panes"),
            agents: self.view("agents"),
            status: self.view("status"),
            layout: self.view("layout"),
        }
    }

    /// The focused pane's visible rows (`App::screen_text`) as JSON.
    fn screen_json(&self) -> String {
        self.app
            .as_ref()
            .and_then(|app| app.screen_text())
            .map(|t| json(&t))
            .unwrap_or_else(|| "null".into())
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Attach {
                layer,
                width,
                height,
                scale,
                reply,
            } => {
                let generation = match self.attach(layer, width, height, scale) {
                    Ok(generation) => generation,
                    Err(err) => {
                        self.fail(format!("attach: {err:#}"));
                        0
                    }
                };
                let _ = reply.send(generation);
            }
            Cmd::Resize {
                generation,
                width,
                height,
                scale,
            } => self.resize(generation, width, height, scale),
            Cmd::Detach { generation, reply } => {
                self.detach(generation);
                let _ = reply.send(());
            }
            Cmd::Render => self.render(),
            Cmd::Animate(on) => {
                self.animating = on;
                if on {
                    self.request_frame();
                }
            }
            Cmd::Stats { reply } => {
                let _ = reply.send(self.stats_json());
            }
            Cmd::Shutdown => {}
            Cmd::Connect { params, painter } => {
                if let Err(err) = self.connect(params, painter) {
                    self.fail(format!("connect: {err:#}"));
                    self.conn = Conn::Disconnected(format!("{err:#}"));
                    self.set_status(&format!("failed: {err:#}"));
                }
            }
            Cmd::Disconnect => {
                self.disconnect();
                self.set_status("disconnected");
            }
            Cmd::Net { serial, dial, net } => self.on_net(serial, dial, net),
            Cmd::Dialed { serial, outcome } => self.on_dialed(serial, outcome),
            Cmd::Attached { serial, outcome } => self.on_attached(serial, outcome),
            Cmd::Key {
                name,
                ctrl,
                alt,
                shift,
            } => {
                let Some(app) = self.app.clone() else {
                    return;
                };
                let dom = DomKey {
                    key: &name,
                    code: "",
                    ctrl,
                    alt,
                    shift,
                    meta: false,
                    composing: false,
                };
                match map_key(&dom) {
                    Some((key, mods)) => {
                        self.stats.inputs_sent += 1;
                        app.key_down(key, mods, shift);
                    }
                    None => self.notify.on_log(format!("key {name:?} has no mapping")),
                }
            }
            Cmd::Text(text) => {
                if let Some(app) = &self.app {
                    if !text.is_empty() {
                        self.stats.inputs_sent += 1;
                        app.text(&text);
                    }
                }
            }
            Cmd::Paste(text) => {
                if let Some(app) = &self.app {
                    self.stats.inputs_sent += 1;
                    app.paste(&text);
                }
            }
            Cmd::Scroll(lines) => {
                if let Some(app) = self.app.clone() {
                    let vp = self.platform.viewport();
                    app.wheel(&WheelInput {
                        x: vp.width / 2.0,
                        y: vp.height / 2.0,
                        // The shell counts lines back into history; the
                        // App counts down the page.
                        delta: WheelDelta::Lines(-(lines as f64)),
                        mods: KeyModifiers::NONE,
                        ctrl: false,
                        trusted: true,
                    });
                }
            }
            Cmd::Composing(on) => {
                self.composing = on;
                if let Some(app) = &self.app {
                    app.composing(on);
                }
            }
            Cmd::View { name, reply } => {
                let _ = reply.send(self.view(&name));
            }
            Cmd::UploadFor(id) => {
                if let Some(app) = &self.app {
                    self.upload_panes.insert(id, (app.focused_pane(), app.server_id()));
                }
                self.upload_hosts.insert(id, self.icons_host.clone());
            }
            Cmd::Upload { id, name, path } => {
                // The pane focused as it was begun; one begun with no App
                // yet, the one focused now.
                if let Some(app) = &self.app {
                    self.upload_panes.entry(id).or_insert_with(|| (app.focused_pane(), app.server_id()));
                }
                let moved = self.upload_hosts.remove(&id).is_some_and(|begun| begun != self.icons_host);
                if moved {
                    self.upload_ended(id, Err("the host changed".into()));
                } else if !self.link.as_ref().is_some_and(|link| link.upload(id, name, path)) {
                    self.upload_ended(id, Err("not connected".into()));
                }
            }
            Cmd::PasteUpload { id, text } => {
                // Not on a server started anew since, or another -- the App
                // may have reconnected to one by itself meanwhile: its pane
                // ids are other terminals'. The upload says where the file
                // went all the same.
                if let (Some((pane, server)), Some(app)) = (self.upload_panes.remove(&id), &self.app) {
                    if server.is_some() && server == app.server_id() {
                        self.stats.inputs_sent += 1;
                        app.paste_into(pane, &text);
                    }
                }
            }
            Cmd::ForgetHostKey => {
                if let Some(link) = &self.link {
                    link.forget_host_key();
                }
            }
            Cmd::ForgetJumpHostKey => {
                if let Some(link) = &self.link {
                    link.forget_jump_host_key();
                }
            }
            Cmd::CheckConnection => {
                if let Some(link) = &self.link {
                    link.probe();
                }
                if let Some(app) = self.app.clone() {
                    app.reconnect_now();
                }
            }
            Cmd::CancelUpload(id) => {
                // Its pane too, for one begun whose file never came.
                self.upload_panes.remove(&id);
                self.upload_hosts.remove(&id);
                if let Some(link) = &self.link {
                    link.cancel_upload(id);
                }
            }
            Cmd::ChromeClick { action, pane, tab } => {
                if let Some(app) = self.app.clone() {
                    let click = thinkterm_web::commands::chrome_click(
                        &action,
                        pane.map(|p| p as usize),
                        tab.map(|t| t as usize),
                    );
                    match click {
                        Some(click) => app.on_chrome_click(click),
                        None => self
                            .notify
                            .on_log(format!("chrome click {action:?} is not a thing")),
                    }
                }
            }
            Cmd::SideClick { kind, id, flag } => {
                if let Some(app) = self.app.clone() {
                    match thinkterm_web::commands::side_click(&kind, id, flag) {
                        Some(click) => app.on_side_click(click),
                        None => self
                            .notify
                            .on_log(format!("side click {kind:?} is not a thing")),
                    }
                }
            }
            Cmd::SideKey { key, value } => {
                if let Some(app) = self.app.clone() {
                    app.on_side_key(&key, value);
                }
            }
            Cmd::ContextMenu { kind, id, reply } => {
                let json = self
                    .app
                    .as_ref()
                    .map(|app| json(&app.context_menu(&kind, &id)))
                    .unwrap_or_else(|| "[]".into());
                let _ = reply.send(json);
            }
            Cmd::MenuAction { id, reply } => {
                let json = self
                    .app
                    .clone()
                    .map(|app| json(&app.menu_action(&id)))
                    .unwrap_or_else(|| "{\"handled\":false,\"copy\":null,\"paste\":false}".into());
                let _ = reply.send(json);
            }
            Cmd::SetSpace(id) => {
                if let Some(app) = &self.app {
                    app.set_space(&id);
                }
            }
            Cmd::TakeOver => {
                if let Some(app) = self.app.clone() {
                    app.take_over();
                }
            }
            Cmd::Pointer { kind, x, y } => {
                if let Some(app) = self.app.clone() {
                    let what = match kind.as_str() {
                        "down" => thinkterm_web::app::Pointer::Down,
                        "move" => thinkterm_web::app::Pointer::Move,
                        _ => thinkterm_web::app::Pointer::Up,
                    };
                    app.pointer(
                        &PointerInput {
                            x,
                            y,
                            button: 0,
                            buttons_down: what != thinkterm_web::app::Pointer::Up,
                            mods: KeyModifiers::NONE,
                        },
                        what,
                    );
                }
            }
            Cmd::Mouse { kind, x, y, button, held, mods } => {
                if let Some(app) = self.app.clone() {
                    let what = match kind.as_str() {
                        "down" => thinkterm_web::app::Pointer::Down,
                        "move" => thinkterm_web::app::Pointer::Move,
                        _ => thinkterm_web::app::Pointer::Up,
                    };
                    app.pointer(&PointerInput { x, y, button, buttons_down: held, mods }, what);
                }
            }
            Cmd::Wheel { x, y, lines } => {
                if let Some(app) = self.app.clone() {
                    app.wheel(&WheelInput {
                        x,
                        y,
                        delta: WheelDelta::Lines(-lines),
                        mods: KeyModifiers::NONE,
                        ctrl: false,
                        trusted: true,
                    });
                }
            }
            Cmd::WheelPx { x, y, px } => {
                if let Some(app) = self.app.clone() {
                    app.wheel(&WheelInput {
                        x,
                        y,
                        delta: WheelDelta::Pixels(px),
                        mods: KeyModifiers::NONE,
                        ctrl: false,
                        trusted: true,
                    });
                }
            }
            Cmd::ScreenText { reply } => {
                let _ = reply.send(self.screen_json());
            }
            Cmd::TextBetween { anchor, head, reply } => {
                let _ = reply.send(self.app.as_ref().and_then(|app| app.text_between(anchor, head)));
            }
            Cmd::SetSelection { anchor, head } => {
                if let Some(app) = self.app.clone() {
                    app.set_selection(anchor, head);
                }
            }
            Cmd::ClearSelection => {
                if let Some(app) = self.app.clone() {
                    app.clear_selection();
                }
            }
            Cmd::Preview { pane, rows } => {
                if let Some(app) = self.app.clone() {
                    let notify = Arc::clone(&self.notify);
                    let task = app.preview_lines(pane, rows);
                    let _ = self.pool.spawner().spawn_local(async move {
                        match task.await {
                            Ok(rows) => notify.on_preview(pane as u32, json(&rows)),
                            Err(err) => notify.on_log(format!("preview of pane {pane}: {err:#}")),
                        }
                    });
                }
            }
            Cmd::ScreenPreview { pane } => {
                if let Some(app) = self.app.clone() {
                    let notify = Arc::clone(&self.notify);
                    let task = app.preview_tab(pane);
                    let _ = self.pool.spawner().spawn_local(async move {
                        match task.await {
                            Ok(screen) => notify.on_preview(pane as u32, json(&screen)),
                            Err(err) => notify.on_log(format!("tab of pane {pane}: {err:#}")),
                        }
                    });
                }
            }
            Cmd::SelectedText { reply } => {
                let _ = reply.send(self.app.as_ref().and_then(|app| app.selected_text()));
            }
            Cmd::StepFont(by) => {
                if let Some(app) = self.app.clone() {
                    app.step_font(by);
                }
            }
            Cmd::SetSetting { key, value } => {
                self.settings.retain(|(k, _)| k != &key);
                self.settings.push((key.clone(), value.clone()));
                if let Some(app) = self.app.clone() {
                    if let Err(err) = app.set_setting(&key, &value) {
                        self.notify.on_log(format!("setting {key}: {err}"));
                    }
                    if key == "terminal-scheme" && !self.palette_tellable() {
                        self.palette_untold = true;
                    }
                }
            }
            Cmd::SetPalette(scheme) => {
                self.palette = scheme.clone();
                if let Some(app) = self.app.clone() {
                    Self::apply_palette(&app, &self.notify, scheme.as_deref());
                    if !self.palette_tellable() {
                        self.palette_untold = true;
                    }
                }
            }
            Cmd::SetFallbackPalette { scheme, unchosen } => {
                self.fallback = scheme.map(|scheme| (scheme, unchosen));
                if let Some(app) = self.app.clone() {
                    Self::apply_fallback(&app, &self.notify, self.fallback.as_ref());
                    if !self.palette_tellable() {
                        self.palette_untold = true;
                    }
                }
            }
            Cmd::SetShown(shown) => {
                if !self.platform.set_shown(shown) {
                    return;
                }
                self.notify.on_log(format!("shown: {shown}"));
                if let Some(app) = self.app.clone() {
                    // As the browser does for a page or a machine coming on
                    // show: what it shows is looked at, sized and listed
                    // again, so a tab it shows is claimed now.
                    app.visibility_changed(shown);
                    if shown {
                        app.resize();
                        app.refresh_layout();
                        self.tell_palette(&app);
                    } else {
                        // A tab zoomed for want of room goes back to its
                        // split while nobody looks at it here.
                        app.leave_one_pane();
                    }
                }
            }
            Cmd::OnePane(one) => {
                if self.platform.set_one_pane(one) {
                    if let Some(app) = self.app.clone() {
                        app.one_pane_changed();
                    }
                    self.views_dirty.set(true);
                }
            }
            Cmd::ViewsRead => self.views_dirty.set(true),
            Cmd::ListViews(listed) => {
                self.views_listed = listed;
                // What changed while nobody looked is made now.
                if listed {
                    self.views_dirty.set(true);
                }
            }
            Cmd::WantScreen(wanted) => {
                self.screen_wanted = wanted;
                // Made at once rather than at the next paint, and sent even
                // when they read as the ones left from before.
                self.screen_dirty = wanted;
                self.screen_fresh = wanted;
            }
        }
    }

    /// A scheme chosen off screen or while the link is down reaches the
    /// server only from a terminal on show, over a link that is up: told
    /// when both hold, kept until then. (Applying it again would not tell
    /// it: the App has it.)
    fn tell_palette(&mut self, app: &Rc<App<MobilePlatform, SshLink>>) {
        if self.palette_untold && self.palette_tellable() {
            self.palette_untold = false;
            app.advise_palette();
        }
    }

    /// The App tells the server a scheme as it is applied.
    fn palette_tellable(&self) -> bool {
        self.platform.shown() && self.link.as_ref().is_some_and(|link| link.is_open())
    }

    fn apply_palette(app: &Rc<App<MobilePlatform, SshLink>>, notify: &Arc<dyn Notify>, scheme: Option<&str>) {
        let palette = match scheme {
            None => None,
            Some(json) => match thinkterm_web::settings::SchemeColors::parse(json).and_then(|c| c.to_palette()) {
                Ok(palette) => Some(palette),
                Err(err) => {
                    notify.on_log(format!("scheme: {err}"));
                    return;
                }
            },
        };
        app.set_terminal_palette(palette);
    }

    fn apply_fallback(
        app: &Rc<App<MobilePlatform, SshLink>>,
        notify: &Arc<dyn Notify>,
        fallback: Option<&(String, Option<String>)>,
    ) {
        let read = |json: &str| {
            thinkterm_web::settings::SchemeColors::parse(json)
                .and_then(|colors| colors.to_palette())
                .map_err(|err| notify.on_log(format!("scheme: {err}")))
                .ok()
        };
        let fallback = match fallback {
            None => None,
            Some((scheme, unchosen)) => {
                let Some(scheme) = read(scheme) else {
                    return;
                };
                Some((scheme, unchosen.as_deref().and_then(read)))
            }
        };
        app.set_fallback_palette(fallback);
    }

    fn view(&self, name: &str) -> String {
        let Some(app) = &self.app else {
            return "null".into();
        };
        match name {
            "tabs" => json(&app.tabs_view()),
            "threads" => json(&app.threads_view()),
            "tree" => json(&app.tree_view()),
            "sidebar" => json(&app.sidebar_view()),
            "navs" => json(&app.navs_view()),
            "panes" => json(&app.panes_view()),
            "agents" => json(&app.agents_view()),
            "status" => json(&app.status_view()),
            "layout" => app.layout_view(),
            "strings" => json(&thinkterm_web::views::strings()),
            _ => "null".into(),
        }
    }

    /// Upload `id` is over: where it is on the host, or why it is not,
    /// for the shell to read (`Core::upload_status`).
    fn upload_ended(&mut self, id: u64, result: Result<String, String>) {
        self.upload_hosts.remove(&id);
        if result.is_err() {
            self.upload_panes.remove(&id);
        }
        if let Ok(mut published) = self.published.lock() {
            if let Some(upload) = published.uploads.get_mut(&id) {
                upload.done = true;
                match result {
                    Ok(path) => upload.path = path,
                    Err(error) => upload.error = error,
                }
            }
        }
    }

    /// Every upload not yet over ends, for `reason`.
    fn end_unfinished_uploads(&mut self, reason: &str) {
        let unfinished: Vec<u64> = self
            .published
            .lock()
            .map(|published| published.uploads.iter().filter(|(_, upload)| !upload.done).map(|(id, _)| *id).collect())
            .unwrap_or_default();
        for id in unfinished {
            self.upload_ended(id, Err(reason.to_string()));
        }
    }

    fn fail(&mut self, message: String) {
        self.stats.errors += 1;
        self.stats.last_error = message.clone();
        self.notify.on_log(message);
    }

    fn set_status(&mut self, status: &str) {
        if self.status != status {
            self.status = status.to_string();
            self.notify.on_status(self.status.clone());
            self.views_dirty.set(true);
        }
    }

    fn request_frame(&mut self) {
        self.stats.frames_requested += 1;
        self.platform.request_frame();
    }

    // ----- connection -----

    fn connect(&mut self, params: ConnectParams, painter: Box<dyn GlyphPainter>) -> Result<()> {
        self.disconnect();
        // No host's cards on another's tabs: the built-in ones until this
        // host's are read. The same host again keeps its own meanwhile.
        let host = (params.ssh.host.clone(), params.ssh.port, params.ssh.user.clone());
        if self.icons_host.as_ref() != Some(&host) {
            self.show_icons(icons::catalog());
            self.icons_host = Some(host);
        }
        self.device_id = params.device_id.clone();
        let mut faces = Vec::new();
        for path in &params.font_paths {
            let bytes = std::fs::read(path).with_context(|| format!("reading the font {path}"))?;
            let name = std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("font")
                .to_string();
            faces.push(Face::new(&name, bytes, 0).with_context(|| name.clone())?);
        }
        anyhow::ensure!(!faces.is_empty(), "no fonts given");
        self.fonts = Some(Rc::new(FontSet::new(faces)?));
        self.size_pt = params.size_pt;
        self.glyph_platform = Some(Rc::new(GlyphSeams::new(painter)));

        let tx = self.cmd_tx.clone();
        let serial = self.link_serial;
        let deliver: Box<dyn Fn(u64) -> Box<dyn Fn(Net) + Send>> = Box::new(move |dial| {
            let tx = tx.clone();
            Box::new(move |net| {
                let _ = tx.send(Cmd::Net { serial, dial, net });
            })
        });
        let link = SshLink::new(params.ssh.clone(), deliver);
        let dial = link.connect();
        self.link = Some(link);
        self.conn = Conn::Connecting;
        self.set_status(&format!(
            "connecting to {}@{}:{}",
            params.ssh.user, params.ssh.host, params.ssh.port
        ));
        let tx = self.cmd_tx.clone();
        self.pool
            .spawner()
            .spawn_local(async move {
                let outcome = dial.await.map_err(|e| format!("{e:#}"));
                let _ = tx.send(Cmd::Dialed { serial, outcome });
            })
            .map_err(|e| anyhow!("spawning the dial: {e}"))?;
        Ok(())
    }

    /// End the connection. The App goes with it; its GPU state comes back
    /// to the core so the next connection (or the demo) can draw. The
    /// surface, if lent, is dropped: the shell re-attaches it by calling
    /// `attach_surface` again, which is what it does on any change anyway.
    fn disconnect(&mut self) {
        // What was on its way goes with the connection: its word that it
        // ended would come from the old link, and be dropped below.
        self.end_unfinished_uploads("disconnected");
        // Invalidate queued completions even if no new connection follows,
        // or the next connect fails before it can create its link.
        self.link_serial += 1;
        self.attaching = false;
        if let Some(link) = self.link.take() {
            link.shutdown();
        }
        if let Some(app) = self.app.take() {
            // A tab it zoomed for want of room is the next App's to put
            // back, should that be on the same server.
            self.one_pane_zoomed = app.one_pane_zoomed().zip(app.server_id()).map(|(zoomed, server)| (server, zoomed));
            // Cut the link's hold on the App, then let every task on the
            // wire see the link gone and finish, so nothing but this
            // handle is left holding it.
            app.retire();
            self.pool.run_until_stalled();
            // The device, the pipeline and the surface come back to the
            // core for the next connection (or the demo).
            match App::into_gpu(app) {
                Ok(gpu) => self.gpu = Some(gpu),
                Err(app) => {
                    // Something still holds the App. It must never draw
                    // again on a window the shell may release: take the
                    // surface from it and unhook its frame handler, then
                    // remake the GPU state on the target still attached.
                    app.with_gpu(|gpu| gpu.detach_surface());
                    self.platform.set_frame_handler(Box::new(|| {}));
                    drop(app);
                    self.gpu = None;
                    self.notify
                        .on_log("the App was still in use; remaking the GPU state".into());
                    self.remake_gpu();
                }
            }
            // Every timer and interval belonged to that App.
            self.platform.clear_timers();
            self.views_dirty.set(true);
        }
        self.conn = Conn::Idle;
    }

    /// A fresh device and swapchain on the surface the shell still lends,
    /// for when the old ones went away with a retired App.
    fn remake_gpu(&mut self) {
        let Some(target) = self.target.as_ref() else {
            return;
        };
        let (layer, width, height) = (target.layer, target.width, target.height);
        let made = self.surface_from_layer(layer).and_then(|surface| {
            futures::executor::block_on(Gpu::from_surface(&self.instance, surface, width, height))
        });
        match made {
            Ok(gpu) => {
                let info = gpu.adapter_info.name.clone();
                self.gpu = Some(gpu);
                self.notify.on_log(format!("gpu ready again: {info}"));
            }
            Err(err) => {
                self.target = None;
                self.notify.on_log(format!(
                    "remaking the GPU state failed: {err:#} -- attach the surface again"
                ));
            }
        }
    }

    fn on_dialed(&mut self, serial: u64, outcome: Result<(), String>) {
        if serial != self.link_serial {
            return;
        }
        match outcome {
            Ok(()) => {
                self.conn = Conn::Attaching;
                self.set_status("connected; attaching");
                self.start_attach();
            }
            Err(err) => {
                self.fail(format!("dial: {err}"));
                self.conn = Conn::Disconnected(err.clone());
                self.set_status(&format!("failed: {err}"));
                self.link = None;
            }
        }
    }

    fn on_net(&mut self, serial: u64, dial: u64, net: Net) {
        if serial != self.link_serial {
            // A thread of an earlier link, saying its goodbyes: the link
            // it speaks for is gone, and the current one must not hear
            // them as its own.
            if let Net::Closed(reason) = net {
                self.notify
                    .on_log(format!("an earlier connection ended: {reason}"));
            }
            return;
        }
        let Some(link) = self.link.clone() else {
            return;
        };
        let earlier = !link.is_dial(dial);
        match net {
            // A dial of this link's since shut down or replaced: what it
            // says of its transport is not the current dial's, and its close
            // would fail the dial now on its way. Its uploads end with it.
            Net::Connected | Net::Data(_) | Net::HostKeyRequired { .. } if earlier => {}
            Net::Closed(reason) if earlier => {
                self.end_unfinished_uploads(&reason);
                self.notify.on_log(format!("an earlier dial ended: {reason}"));
            }
            Net::Connected => {
                link.opened();
                // A scheme still untold for want of a link goes now.
                if let Some(app) = self.app.clone() {
                    self.tell_palette(&app);
                }
            }
            Net::Data(bytes) => {
                self.stats.bytes_in += bytes.len() as u64;
                if let Err(err) = link.feed(&bytes) {
                    self.fail(format!("link: {err:#}"));
                    link.closed(format!("{err:#}"));
                }
            }
            Net::HostKeyRequired {
                request_id,
                fingerprint,
                jump,
            } => self.notify.on_host_key_required(request_id, fingerprint, jump),
            Net::Stderr(text) => self.notify.on_log(format!("remote: {}", text.trim_end())),
            Net::Exit(code) => self
                .notify
                .on_log(format!("remote command exited with {code}")),
            Net::TabIcons(Ok(cards)) => self.show_icons(cards),
            Net::TabIcons(Err(err)) => self.notify.on_log(format!("tab icons: {err}")),
            Net::UploadProgress { id, sent, total } => {
                if let Ok(mut published) = self.published.lock() {
                    if let Some(upload) = published.uploads.get_mut(&id) {
                        (upload.sent, upload.total) = (sent, total);
                    }
                }
            }
            Net::Uploaded { id, result } => self.upload_ended(id, result),
            Net::Closed(reason) => {
                // What was on its way went with the connection.
                self.end_unfinished_uploads(&reason);
                if self.app.is_none() {
                    self.conn = Conn::Disconnected(reason.clone());
                    self.set_status(&format!("disconnected: {reason}"));
                }
                // With an App up, its close handler takes it from here:
                // it schedules the redial and reattaches on the same pane.
                link.closed(reason);
            }
        }
    }

    /// Put `cards` on the tabs, for the App and the shell's drawing.
    fn show_icons(&mut self, cards: thinkterm_tab_icons::WireCatalog) {
        if !self.icons.set(cards) {
            return;
        }
        if let Some(app) = &self.app {
            if let Err(err) = app.set_tab_icons(&self.icons.json()) {
                self.notify.on_log(format!("tab icons: {err}"));
            }
        }
        // The shell redraws the cards when it sees the views move.
        self.views_dirty.set(true);
    }

    /// The link is up: attach to the server's active pane at the grid the
    /// surface holds (measured with the glyph metrics for this screen).
    fn start_attach(&mut self) {
        let (Some(link), Some(fonts)) = (self.link.clone(), self.fonts.clone()) else {
            return;
        };
        if self.attaching {
            // The one in flight attaches to whatever surface is there
            // when it lands; the App resizes to it then.
            return;
        }
        if self.gpu.is_none() {
            // Not a failure: the surface's arrival attaches.
            self.notify.on_log("connected before the surface; attaching when it comes".into());
            return;
        }
        let scale = self.target.as_ref().map(|t| t.scale).unwrap_or(1.0);
        let dpi = (self.platform.units_per_inch() * scale) as u32;
        let metrics = match fonts.metrics(self.size_pt, dpi) {
            Ok(m) => thinkterm_web::glyphs::RenderMetrics::with_font_metrics(&m),
            Err(err) => {
                self.fail(format!("font metrics: {err:#}"));
                return;
            }
        };
        let (cw, ch) = (
            metrics.cell_size.width as u32,
            metrics.cell_size.height as u32,
        );
        let size = self.target.as_ref().and_then(|t| {
            grid_for(t.width.saturating_sub(2 * cw), t.height, cw, ch).map(|(cols, rows)| {
                wezterm_term::TerminalSize {
                    rows,
                    cols,
                    pixel_width: cols * cw as usize,
                    pixel_height: rows * ch as usize,
                    dpi,
                }
            })
        });
        self.notify.on_log(format!(
            "cell {cw}x{ch} px at {} pt, {dpi} dpi; reporting {:?}",
            self.size_pt,
            size.map(|s| (s.cols, s.rows))
        ));
        // The identity the server compares leases against, derived from
        // the install's id so it survives a relaunch.
        let (epoch, id) = stable_identity(&self.device_id);
        let me = thinkterm_proto::ClientId {
            hostname: if cfg!(target_os = "android") { "android" } else { "ios" }.into(),
            username: "mobile".into(),
            pid: 0,
            epoch,
            id,
            ssh_auth_sock: None,
        };
        let tx = self.cmd_tx.clone();
        let serial = self.link_serial;
        self.attaching = true;
        let spawned = self.pool.spawner().spawn_local(async move {
            let outcome = attach(&link, size, 0, me).await.map_err(|e| {
                match e.downcast_ref::<ProtocolMismatch>() {
                    Some(mismatch) => mismatch_text(mismatch),
                    None => format!("{e:#}"),
                }
            });
            let _ = tx.send(Cmd::Attached { serial, outcome });
        });
        if let Err(err) = spawned {
            self.attaching = false;
            self.fail(format!("spawning the handshake: {err}"));
        }
    }

    fn on_attached(&mut self, serial: u64, outcome: Result<Attached, String>) {
        if serial != self.link_serial {
            return;
        }
        self.attaching = false;
        let attached = match outcome {
            Ok(attached) => attached,
            Err(err) => {
                self.fail(format!("attach: {err}"));
                // A link that closed under the handshake already said why;
                // that reason beats "closed before the server answered".
                let reason = match &self.conn {
                    Conn::Disconnected(reason) => reason.clone(),
                    _ => err.clone(),
                };
                self.conn = Conn::Disconnected(reason.clone());
                self.set_status(&format!("failed: {reason}"));
                if let Some(link) = self.link.take() {
                    link.shutdown();
                }
                return;
            }
        };
        // The GPU state is taken only once the rest is known to be there:
        // taken inside a pattern that then fails to match, it would be
        // dropped with the match, and the target left over it would draw
        // on nothing.
        let (Some(link), Some(fonts), Some(glyph_platform), true) = (
            self.link.clone(),
            self.fonts.clone(),
            self.glyph_platform.clone(),
            self.gpu.is_some(),
        ) else {
            self.fail("attached without a link, fonts or a GPU".into());
            return;
        };
        let gpu = self.gpu.take().expect("checked just above");
        let scale = self.target.as_ref().map(|t| t.scale).unwrap_or(1.0);
        let dpi = (self.platform.units_per_inch() * scale) as u32;
        let side = 1024u32.min(gpu.max_texture_dimension());
        let glyphs = GpuTexture::new(&gpu.device, Arc::clone(&gpu.queue), side, side)
            .map(Rc::new)
            .and_then(|texture| {
                GlyphCache::new(
                    Rc::clone(&fonts),
                    self.size_pt,
                    dpi,
                    texture,
                    Rc::from(""),
                    glyph_platform,
                )
            });
        let glyphs = match glyphs {
            Ok(g) => g,
            Err(err) => {
                self.fail(format!("glyph cache: {err:#}"));
                self.gpu = Some(gpu);
                return;
            }
        };
        let (cw, ch) = (
            glyphs.metrics.cell_size.width as u32,
            glyphs.metrics.cell_size.height as u32,
        );
        let (cols, rows) = self
            .target
            .as_ref()
            .and_then(|t| grid_for(t.width.saturating_sub(2 * cw), t.height, cw, ch))
            .unwrap_or((attached.dims.cols, attached.dims.viewport_rows));

        let host = Arc::new(AppHost::new(Rc::clone(&self.platform), link.clone()));
        let images = Arc::new(thinkterm_session::Lock::new(
            thinkterm_session::images::ImageStore::default(),
        ));
        let remote_tab_id = Arc::new(std::sync::atomic::AtomicUsize::new(attached.tab_id));
        let session = build_session(
            &host,
            &images,
            &remote_tab_id,
            attached.pane_id,
            attached.dims,
            &attached.title,
            attached.alt_screen,
        );
        let app = App::new(Setup {
            platform: Rc::clone(&self.platform),
            link: link.clone(),
            host: Arc::clone(&host),
            images,
            remote_tab_id,
            pane: PaneCell::new(session, &attached.title),
            gpu,
            glyphs,
            fonts,
            pane_id: attached.pane_id,
            tab_id: attached.tab_id,
            window_id: attached.window_id,
            workspace: attached.workspace.clone(),
            dpr: scale,
            cols,
            rows,
            font_pinned: true,
            languages: vec![],
            server_version: attached.server_version.clone(),
        });
        host.events.set_wake(app.wake());
        {
            let platform = Rc::clone(&app.platform);
            host.events.set_bell(Rc::new(move |_| platform.bell()));
        }
        {
            let app = Rc::clone(&app);
            link.set_push_handler(Box::new(move |pdu| app.on_push(pdu)));
        }
        {
            let app = Rc::clone(&app);
            link.set_close_handler(Box::new(move |reason| app.on_close(reason)));
        }
        {
            // Inside the App's own call: only noted here, the views are
            // made between commands (`publish_views`).
            let dirty = Rc::clone(&self.views_dirty);
            app.set_on_change(Rc::new(move || dirty.set(true)));
        }
        if !self.platform.shown() {
            // Made behind the screen (a reconnect while the app is in the
            // background): it looks at nothing until it is shown.
            app.visibility_changed(false);
        }
        app.composing(self.composing);
        for (key, value) in &self.settings {
            if let Err(err) = app.set_setting(key, value) {
                self.notify.on_log(format!("setting {key}: {err}"));
            }
        }
        if let Some(scheme) = self.palette.clone() {
            Self::apply_palette(&app, &self.notify, Some(&scheme));
        }
        if self.fallback.is_some() {
            Self::apply_fallback(&app, &self.notify, self.fallback.as_ref());
        }
        // Made off screen, it told the server no scheme it was given. A new
        // connection holds no scheme of before to undo: the desktop's is
        // its own already.
        self.palette_untold = (self.palette.is_some() || self.fallback.is_some()) && !self.platform.shown();
        if let Err(err) = app.set_tab_icons(&self.icons.json()) {
            self.notify.on_log(format!("tab icons: {err}"));
        }
        // The tab the App before zoomed for want of room, on this server.
        if let Some((_, zoomed)) = self.one_pane_zoomed.take().filter(|(server, _)| *server == attached.server_id) {
            app.keep_one_pane_zoomed(zoomed);
        }
        app.fetch_tree();
        app.hide_status();
        app.refresh_layout();
        app.poll_layout(5_000);
        app.resize();
        app.request_frame();
        self.notify.on_log(format!(
            "attached to pane {} in tab {} on {} ({cols}x{rows}), server {}",
            attached.pane_id, attached.tab_id, attached.server_version, attached.server_id
        ));
        self.app = Some(app);
        self.views_dirty.set(true);
        self.conn = Conn::Ready;
        self.set_status(&format!("pane {} · {}", attached.pane_id, attached.title));
    }

    // ----- surface -----

    fn attach(&mut self, layer: usize, width: u32, height: u32, scale: f64) -> Result<u64> {
        if self.target.is_some() {
            anyhow::bail!("a surface is already attached; detach it first");
        }
        let surface = self.surface_from_layer(layer)?;
        if self.adapter.is_none() {
            let adapter = futures::executor::block_on(self.instance.request_adapter(
                &wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: Some(&surface),
                    force_fallback_adapter: false,
                },
            ))
            .context("requesting a GPU adapter")?;
            self.adapter = Some(adapter);
        }
        let adapter = self.adapter.as_ref().expect("adapter just made");
        match (&self.app, &mut self.gpu) {
            (Some(app), _) => {
                app.with_gpu(|g| g.attach_surface(adapter, surface, width, height))?
            }
            (None, Some(gpu)) => gpu.attach_surface(adapter, surface, width, height)?,
            (None, None) => {
                let gpu = futures::executor::block_on(Gpu::from_surface(
                    &self.instance,
                    surface,
                    width,
                    height,
                ))?;
                let info = gpu.adapter_info.name.clone();
                self.gpu = Some(gpu);
                self.notify.on_log(format!("gpu ready: {info}"));
            }
        }
        self.platform.set_viewport(width, height, scale);
        let generation = self.next_generation;
        self.next_generation += 1;
        self.target = Some(Target {
            generation,
            layer,
            width,
            height,
            scale,
        });
        self.stats.attaches += 1;
        self.stats.generation = generation;
        self.stats.width = width;
        self.stats.height = height;
        self.stats.scale = scale;
        self.notify.on_log(format!(
            "attached generation {generation}: {width}x{height} @{scale}"
        ));
        if let Some(app) = self.app.clone() {
            app.resize();
            app.request_frame();
        } else if matches!(self.conn, Conn::Attaching) && self.link.is_some() {
            // The transport was up before the surface: a fast loopback
            // dial beats the first layout. The attach waited for this.
            self.start_attach();
        } else {
            self.request_frame();
        }
        Ok(generation)
    }

    #[cfg(target_vendor = "apple")]
    fn surface_from_layer(&self, layer: usize) -> Result<wgpu::Surface<'static>> {
        if layer == 0 {
            anyhow::bail!("null layer");
        }
        // SAFETY: the shell passes an unretained CAMetalLayer pointer and
        // guarantees the layer outlives the surface: it calls
        // `detach_surface` -- which drops this surface before returning --
        // ahead of releasing the layer.
        unsafe {
            self.instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
                    layer as *mut std::ffi::c_void,
                ))
        }
        .context("creating the Metal surface")
    }

    /// `layer` is an `ANativeWindow*` the shell took from its Surface
    /// (`NativeWindow.fromSurface`), which it keeps alive until after
    /// `detach_surface`.
    #[cfg(target_os = "android")]
    fn surface_from_layer(&self, layer: usize) -> Result<wgpu::Surface<'static>> {
        use raw_window_handle::{AndroidDisplayHandle, AndroidNdkWindowHandle, RawDisplayHandle, RawWindowHandle};
        let Some(window) = std::ptr::NonNull::new(layer as *mut std::ffi::c_void) else {
            anyhow::bail!("null window");
        };
        // SAFETY: the shell guarantees the window outlives the surface,
        // dropping it only after `detach_surface` returned.
        unsafe {
            self.instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: RawDisplayHandle::Android(AndroidDisplayHandle::new()),
                raw_window_handle: RawWindowHandle::AndroidNdk(AndroidNdkWindowHandle::new(window)),
            })
        }
        .context("creating the Vulkan surface")
    }

    #[cfg(not(any(target_vendor = "apple", target_os = "android")))]
    fn surface_from_layer(&self, _layer: usize) -> Result<wgpu::Surface<'static>> {
        anyhow::bail!("no surface source on this platform yet")
    }

    fn resize(&mut self, generation: u64, width: u32, height: u32, scale: f64) {
        let Some(target) = self.target.as_mut() else {
            return;
        };
        if target.generation != generation {
            self.notify.on_log(format!(
                "resize for stale generation {generation} ignored (current {})",
                target.generation
            ));
            return;
        }
        let (width, height) = (width.max(1), height.max(1));
        if (width, height, scale) == (target.width, target.height, target.scale) {
            return;
        }
        target.width = width;
        target.height = height;
        target.scale = scale;
        self.platform.set_viewport(width, height, scale);
        self.stats.resizes += 1;
        self.stats.width = width;
        self.stats.height = height;
        self.stats.scale = scale;
        if let Some(app) = self.app.clone() {
            app.resize();
        } else if let Some(gpu) = self.gpu.as_mut() {
            gpu.resize(width, height);
            self.request_frame();
        }
    }

    fn detach(&mut self, generation: u64) {
        match self.target.take() {
            Some(target) if target.generation == generation => {
                if let Some(app) = &self.app {
                    app.with_gpu(|g| g.detach_surface());
                } else if let Some(gpu) = self.gpu.as_mut() {
                    gpu.detach_surface();
                }
                self.stats.detaches += 1;
                self.notify
                    .on_log(format!("detached generation {generation}"));
            }
            Some(target) => {
                let current = target.generation;
                self.target = Some(target);
                self.notify.on_log(format!(
                    "detach for stale generation {generation} ignored (current {current})"
                ));
            }
            None => self
                .notify
                .on_log(format!("detach {generation}: nothing attached")),
        }
    }

    // ----- frames -----

    fn render(&mut self) {
        let started = Instant::now();
        if self.app.is_some() {
            if self.platform.run_frame() {
                self.stats.frames += 1;
                self.stats.last_frame_us = started.elapsed().as_micros();
                // Output changes the rows without changing a view.
                self.screen_dirty = true;
            }
            return;
        }
        // No App yet: a bare ground, so the surface path can be seen
        // working without drawing anything that looks like content.
        self.platform.run_frame();
        let (Some(_), Some(gpu)) = (self.target.as_ref(), self.gpu.as_mut()) else {
            return;
        };
        if let Err(err) = gpu.draw_batches(&[], [0.07, 0.07, 0.09, 1.0], 0) {
            self.fail(format!("render: {err:#}"));
            return;
        }
        self.stats.frames += 1;
        self.stats.last_frame_us = started.elapsed().as_micros();
    }

    fn stats_json(&self) -> String {
        let s = &self.stats;
        let layout = self
            .app
            .as_ref()
            .map(|a| a.layout_view())
            .unwrap_or_else(|| "null".into());
        // The App's own grid is in its layout JSON as "canvas":[cols,rows].
        let grid = layout
            .split("\"canvas\":[")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .map(|cr| cr.replace(',', "x"))
            .unwrap_or_default();
        format!(
            "{{\"attached\":{},\"generation\":{},\"size\":\"{}x{}@{}\",\"attaches\":{},\"detaches\":{},\"resizes\":{},\"frames\":{},\"frames_requested\":{},\"last_frame_us\":{},\"phase\":{},\"animating\":{},\"errors\":{},\"last_error\":{:?},\"conn\":{:?},\"grid\":{:?},\"bytes_in\":{},\"inputs_sent\":{},\"composing\":{},\"layout\":{}}}",
            self.target.is_some(),
            s.generation,
            s.width,
            s.height,
            s.scale,
            s.attaches,
            s.detaches,
            s.resizes,
            s.frames,
            s.frames_requested,
            s.last_frame_us,
            s.phase,
            self.animating,
            s.errors,
            s.last_error,
            format!("{:?}", self.conn),
            grid,
            s.bytes_in,
            s.inputs_sent,
            self.composing,
            layout,
        )
    }
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|err| format!("{{\"error\":{:?}}}", err.to_string()))
}


/// What the phone says when the host's ThinkTerm is another release, on
/// the first connection and on a reconnect (`MobilePlatform`). The two
/// speak one protocol version only, so one of them has to move to the
/// other's release; the numbers only go up, so they say which. An iPhone
/// gets the app from TestFlight.
pub(crate) fn mismatch_text(mismatch: &ProtocolMismatch) -> String {
    let host = &mismatch.server_version;
    if mismatch.theirs > mismatch.ours {
        let update = if cfg!(target_os = "ios") {
            "Update the app in TestFlight to the same release"
        } else {
            "Update the app to the same release"
        };
        format!("the host runs a newer ThinkTerm ({host}) than this app. {update}.")
    } else {
        format!(
            "the host runs an older ThinkTerm ({host}) than this app. Update ThinkTerm on the host to the app's release."
        )
    }
}

/// Two numbers from the install's id (FNV-1a over it, twice), for the
/// client identity's epoch and id fields.
fn stable_identity(device_id: &str) -> (u64, usize) {
    fn fnv(bytes: &[u8], seed: u64) -> u64 {
        let mut h = seed ^ 0xcbf2_9ce4_8422_2325;
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h
    }
    let epoch = fnv(device_id.as_bytes(), 0) & 0xffff_ffff;
    let id = (fnv(device_id.as_bytes(), 0x9e37_79b9_7f4a_7c15) & 0x7fff_ffff) as usize;
    (epoch, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Says nothing, and counts `on_change`.
    struct QuietNotify(Arc<std::sync::atomic::AtomicUsize>);

    impl Notify for QuietNotify {
        fn on_frame_needed(&self) {}
        fn on_status(&self, _: String) {}
        fn on_log(&self, _: String) {}
        fn on_title(&self, _: String) {}
        fn on_change(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn on_clipboard(&self, _: String) {}
        fn on_focus_input(&self) {}
        fn on_ime_anchor(&self, _: f64, _: f64, _: f64, _: f64) {}
        fn on_host_key_required(&self, _: String, _: String, _: bool) {}
        fn on_published(&self, _: String, _: String) {}
        fn on_bell(&self) {}
        fn on_preview(&self, _: u32, _: String) {}
    }

    fn state() -> State {
        state_with(&Arc::default(), true)
    }

    fn state_with(changes: &Arc<std::sync::atomic::AtomicUsize>, views_read: bool) -> State {
        let (tx, _) = std::sync::mpsc::channel();
        State::new(
            tx,
            Box::new(QuietNotify(Arc::clone(changes))),
            Arc::new(Mutex::new(Published::default())),
            Arc::new(AtomicBool::new(views_read)),
            Arc::new(icons::Shown::new(icons::catalog())),
        )
    }

    #[test]
    fn a_protocol_mismatch_says_which_side_to_update() {
        let mismatch = |theirs| ProtocolMismatch {
            ours: 89,
            theirs,
            server_version: "0.5.2".into(),
        };
        let newer = mismatch_text(&mismatch(90));
        assert!(newer.contains("newer ThinkTerm (0.5.2)"), "{newer}");
        assert!(newer.contains("Update the app"), "{newer}");
        let older = mismatch_text(&mismatch(88));
        assert!(older.contains("older ThinkTerm (0.5.2)"), "{older}");
        assert!(older.contains("on the host"), "{older}");

        // Through anyhow, as the handshake returns it.
        let err: anyhow::Error = mismatch(90).into();
        let err = err.context("attach");
        assert!(err.downcast_ref::<ProtocolMismatch>().is_some());
    }

    #[test]
    fn views_go_out_once_a_frame_and_only_after_a_change() {
        let changes = Arc::default();
        let mut state = state_with(&changes, true);
        state.views_dirty.set(false);
        state.views_made = Instant::now() - VIEWS_EVERY;
        state.publish_views();
        assert_eq!(state.published.lock().unwrap().views.revision, 0, "nothing changed");

        state.views_dirty.set(true);
        state.views_made = Instant::now();
        state.publish_views();
        assert_eq!(state.published.lock().unwrap().views.revision, 0, "a frame has not passed");

        state.views_made = Instant::now() - VIEWS_EVERY;
        state.publish_views();
        let views = state.published.lock().unwrap().views.clone();
        assert_eq!(views.revision, 1);
        assert_eq!(views.tabs, "null", "no App, no views");
        assert!(!state.views_dirty.get());
        assert_eq!(changes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn views_wait_while_the_terminal_is_off_screen() {
        let changes = Arc::default();
        let mut state = state_with(&changes, true);
        state.platform.set_shown(false);
        state.views_dirty.set(true);
        state.views_made = Instant::now() - VIEWS_EVERY;
        state.publish_views();
        assert_eq!(state.published.lock().unwrap().views.revision, 0, "none made off screen");
        assert!(state.views_dirty.get(), "the change waits");
        assert!(state.wait(Instant::now()) >= Duration::from_secs(60), "and the loop sleeps meanwhile");

        state.platform.set_shown(true);
        assert_eq!(state.wait(Instant::now()), Duration::ZERO, "owed once it is back");
        state.publish_views();
        assert_eq!(state.published.lock().unwrap().views.revision, 1, "made once it is back");
    }

    #[test]
    fn a_list_of_its_threads_keeps_the_views_current_off_screen() {
        let changes = Arc::default();
        let mut state = state_with(&changes, true);
        state.platform.set_shown(false);
        state.views_dirty.set(false);
        state.handle(Cmd::ListViews(true));
        assert!(state.views_dirty.get(), "what changed unseen is made as the list comes up");
        state.views_made = Instant::now() - VIEWS_EVERY;
        assert_eq!(state.wait(Instant::now()), Duration::ZERO);
        state.publish_views();
        assert_eq!(state.published.lock().unwrap().views.revision, 1, "made with the terminal off screen");
        // The rows are the terminal's: none are made for a list.
        state.screen_wanted = true;
        state.screen_dirty = true;
        state.publish_views();
        assert!(!state.screen_dirty);
        assert_eq!(state.published.lock().unwrap().screen_revision, 0);

        state.handle(Cmd::ListViews(false));
        state.views_dirty.set(true);
        state.views_made = Instant::now() - VIEWS_EVERY;
        state.publish_views();
        assert_eq!(state.published.lock().unwrap().views.revision, 1, "and wait again once the list is gone");
    }

    #[test]
    fn a_shell_that_reads_no_views_is_made_none_but_still_told() {
        let changes = Arc::default();
        let mut state = state_with(&changes, false);
        state.views_made = Instant::now() - VIEWS_EVERY;
        state.publish_views();
        assert_eq!(state.published.lock().unwrap().views.revision, 0);
        assert_eq!(changes.load(Ordering::Relaxed), 1, "it pulls each view itself");
        state.handle(Cmd::ViewsRead);
        assert!(state.views_dirty.get(), "the first read has them made");
    }

    #[test]
    fn rows_are_kept_only_while_wanted_and_tell_nothing() {
        let changes = Arc::default();
        let mut state = state_with(&changes, true);
        state.views_dirty.set(false);
        state.screen_dirty = true;
        state.views_made = Instant::now() - VIEWS_EVERY;
        state.publish_views();
        assert!(!state.screen_dirty, "a paint makes no rows nobody wants");
        state.handle(Cmd::WantScreen(true));
        assert!(state.screen_wanted && state.screen_dirty, "wanted rows are made at once");
        state.publish_views();
        assert!(!state.screen_dirty);
        assert_eq!(changes.load(Ordering::Relaxed), 0, "rows alone come without on_change");
    }

    #[test]
    fn a_status_change_marks_the_views_stale() {
        let mut state = state();
        state.views_dirty.set(false);
        state.set_status("connecting to example");
        assert!(state.views_dirty.get());
    }

    #[test]
    fn shown_reaches_the_platform_once_per_change() {
        let mut state = state();
        assert!(state.platform.shown(), "shown until the shell says otherwise");
        state.handle(Cmd::SetShown(false));
        assert!(!state.platform.shown());
        assert!(!state.platform.set_shown(false), "a repeat is no change");
        state.handle(Cmd::SetShown(true));
        assert!(state.platform.shown());
    }

    fn attached() -> Attached {
        Attached {
            pane_id: 1,
            tab_id: 2,
            window_id: 3,
            workspace: String::new(),
            title: String::new(),
            dims: Default::default(),
            alt_screen: false,
            server_version: String::new(),
            server_id: String::new(),
        }
    }

    fn old_completions(serial: u64) -> Vec<Cmd> {
        vec![
            Cmd::Dialed { serial, outcome: Ok(()) },
            Cmd::Dialed { serial, outcome: Err("old dial".into()) },
            Cmd::Attached { serial, outcome: Ok(attached()) },
            Cmd::Attached { serial, outcome: Err("old handshake".into()) },
        ]
    }

    #[test]
    fn disconnect_ignores_queued_completions_without_a_successor() {
        let mut state = state();
        let old = state.link_serial;
        state.handle(Cmd::Disconnect);
        for cmd in old_completions(old) {
            state.handle(cmd);
            assert_eq!(state.conn, Conn::Idle);
            assert_eq!(state.status, "disconnected");
            assert_eq!(state.stats.errors, 0);
            assert!(!state.attaching);
            assert!(state.link.is_none());
        }
    }

    #[test]
    fn old_completions_cannot_change_a_successor_or_clear_its_handshake() {
        let mut state = state();
        let old = state.link_serial;
        state.handle(Cmd::Disconnect);
        // Install a successor without opening a real SSH connection or GPU.
        let link = crate::link::test_link();
        link.opened();
        state.link = Some(link.clone());
        state.conn = Conn::Connecting;
        state.set_status("new connection");
        state.attaching = true;
        for cmd in old_completions(old) {
            state.handle(cmd);
            assert_eq!(state.conn, Conn::Connecting);
            assert_eq!(state.status, "new connection");
            assert_eq!(state.stats.errors, 0);
            assert!(state.attaching);
            assert!(state.link.is_some());
            assert!(link.is_open());
        }
        // The current generation must still deliver failures normally.
        state.handle(Cmd::Attached {
            serial: state.link_serial,
            outcome: Err("current handshake".into()),
        });
        assert_eq!(state.conn, Conn::Disconnected("current handshake".into()));
        assert!(!state.attaching);
        assert!(state.link.is_none());
        assert!(!link.is_open());
    }

    #[test]
    fn a_dial_given_up_cannot_fail_the_one_after_it() {
        let mut state = state();
        let serial = state.link_serial;
        let link = crate::link::test_link();
        let (first, _) = link.begin_dial();
        link.opened();
        state.link = Some(link.clone());
        // The App lets the reconnect go, then dials again on the same link
        // before the first dial's thread has said goodbye.
        link.shutdown();
        let (second, mut answer) = link.begin_dial();
        state.handle(Cmd::Net { serial, dial: first, net: Net::Connected });
        state.handle(Cmd::Net { serial, dial: first, net: Net::Closed("let go".into()) });
        assert!(matches!(answer.try_recv(), Ok(None)), "the second dial still waits for its own");
        assert!(!link.is_open());
        // Its own close still ends it.
        state.handle(Cmd::Net { serial, dial: second, net: Net::Closed("refused".into()) });
        assert!(matches!(answer.try_recv(), Ok(Some(Err(_)))));
    }
}
