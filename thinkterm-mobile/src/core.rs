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

use crate::link::SshLink;
use crate::painter::{GlyphPainter, GlyphSeams};
use crate::platform::MobilePlatform;
use crate::ssh::{Net, SshParams};
use crate::Notify;
use anyhow::{anyhow, Context, Result};
use futures::executor::LocalPool;
use futures::task::LocalSpawnExt;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thinkterm_font_core::FontShaper as _;
use thinkterm_font_web::{Face, FontSet};
use thinkterm_render::pipeline::GpuTexture;
use thinkterm_web::app::{build_session, grid_for, App, PaneCell, Setup};
use thinkterm_web::attach::{attach, Attached};
use thinkterm_web::glyphs::GlyphCache;
use thinkterm_web::gpu::Gpu;
use thinkterm_web::host::AppHost;
use thinkterm_web::keymap::{map_key, DomKey};
use thinkterm_web::platform::{Link, Platform, PointerInput, WheelDelta, WheelInput};
use wezterm_term::KeyModifiers;

pub type MobileApp = App<MobilePlatform, SshLink>;

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
    /// so a retiring thread's last words never reach its successor.
    Net { serial: u64, net: Net },
    /// The first dial finished.
    Dialed(Result<(), String>),
    /// The attach handshake finished.
    Attached(Result<Attached, String>),
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
    SelectedText {
        reply: Sender<Option<String>>,
    },
    SetSetting {
        key: String,
        value: String,
    },
    SetPalette(Option<String>),
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
    /// The shell's preferences, applied to each App as it is made.
    settings: Vec<(String, String)>,
    /// The chosen scheme's colours as JSON, applied to every App made.
    palette: Option<String>,
}

pub fn run(cmd_tx: Sender<Cmd>, rx: Receiver<Cmd>, notify: Box<dyn Notify>) {
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
    let mut state = State {
        cmd_tx,
        instance,
        adapter: None,
        gpu: None,
        app: None,
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
        settings: Vec::new(),
        palette: None,
    };
    state.notify.on_log("core thread up".into());
    state.set_status("idle");

    loop {
        let now = Instant::now();
        let mut wait = Duration::from_secs(3600);
        if state.animating && state.target.is_some() && state.app.is_none() {
            wait = Duration::from_millis(100);
        }
        if let Some(due) = state.platform.next_deadline() {
            wait = wait.min(due.saturating_duration_since(now));
        }
        match rx.recv_timeout(wait) {
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
    }
    state.disconnect();
    state.notify.on_log("core thread down".into());
}

impl State {
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
            Cmd::Net { serial, net } => self.on_net(serial, net),
            Cmd::Dialed(outcome) => self.on_dialed(outcome),
            Cmd::Attached(outcome) => self.on_attached(outcome),
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
                let json = self
                    .app
                    .as_ref()
                    .and_then(|app| app.screen_text())
                    .map(|t| json(&t))
                    .unwrap_or_else(|| "null".into());
                let _ = reply.send(json);
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
                }
            }
            Cmd::SetPalette(scheme) => {
                self.palette = scheme.clone();
                if let Some(app) = self.app.clone() {
                    Self::apply_palette(&app, &self.notify, scheme.as_deref());
                }
            }
        }
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
            "status" => json(&app.status_view()),
            "layout" => app.layout_view(),
            "strings" => json(&thinkterm_web::views::strings()),
            _ => "null".into(),
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
        }
    }

    fn request_frame(&mut self) {
        self.stats.frames_requested += 1;
        self.platform.request_frame();
    }

    // ----- connection -----

    fn connect(&mut self, params: ConnectParams, painter: Box<dyn GlyphPainter>) -> Result<()> {
        self.disconnect();
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
        self.link_serial += 1;
        let serial = self.link_serial;
        let deliver: Box<dyn Fn() -> Box<dyn Fn(Net) + Send>> = Box::new(move || {
            let tx = tx.clone();
            Box::new(move |net| {
                let _ = tx.send(Cmd::Net { serial, net });
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
                let _ = tx.send(Cmd::Dialed(outcome));
            })
            .map_err(|e| anyhow!("spawning the dial: {e}"))?;
        Ok(())
    }

    /// End the connection. The App goes with it; its GPU state comes back
    /// to the core so the next connection (or the demo) can draw. The
    /// surface, if lent, is dropped: the shell re-attaches it by calling
    /// `attach_surface` again, which is what it does on any change anyway.
    fn disconnect(&mut self) {
        self.attaching = false;
        if let Some(link) = self.link.take() {
            link.shutdown();
        }
        if let Some(app) = self.app.take() {
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

    fn on_dialed(&mut self, outcome: Result<(), String>) {
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

    fn on_net(&mut self, serial: u64, net: Net) {
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
        match net {
            Net::Connected => link.opened(),
            Net::Data(bytes) => {
                self.stats.bytes_in += bytes.len() as u64;
                if let Err(err) = link.feed(&bytes) {
                    self.fail(format!("link: {err:#}"));
                    link.closed(format!("{err:#}"));
                }
            }
            Net::HostKey(fingerprint) => self.notify.on_host_key(fingerprint),
            Net::Stderr(text) => self.notify.on_log(format!("remote: {}", text.trim_end())),
            Net::Exit(code) => self
                .notify
                .on_log(format!("remote command exited with {code}")),
            Net::Closed(reason) => {
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
        self.attaching = true;
        let spawned = self.pool.spawner().spawn_local(async move {
            let outcome = attach(&link, size, 0, me)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Cmd::Attached(outcome));
        });
        if let Err(err) = spawned {
            self.attaching = false;
            self.fail(format!("spawning the handshake: {err}"));
        }
    }

    fn on_attached(&mut self, outcome: Result<Attached, String>) {
        self.attaching = false;
        let attached = match outcome {
            Ok(attached) => attached,
            Err(err) => {
                self.fail(format!("attach: {err}"));
                self.conn = Conn::Disconnected(err.clone());
                self.set_status(&format!("attach failed: {err}"));
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
            let notify = Arc::clone(&self.notify);
            app.set_on_change(Rc::new(move || notify.on_change()));
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
