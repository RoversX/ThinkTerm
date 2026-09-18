//! The core thread: owns the wgpu device, the current surface, the link to
//! the server and the one pane the S0.5 slice shows. The shape is the one
//! the plan fixes: device and pipeline outlive any surface; the surface is
//! optional and carries a generation; drawing is a request the shell
//! fulfils from its display callback; content changes -- the server's
//! output, or the demo's clock -- only *ask* for a frame.
//!
//! The thread drives a `LocalPool` for the session's futures (the request
//! answers, the input drain, the render-delta drain) and runs it after
//! every event it handles. Nothing here waits on the shell.

use crate::host::{Config, Events, MobileClock, MobileHost, Spawn};
use crate::link::SshLink;
use crate::painter::{GlyphPainter, MobilePlatform};
use crate::ssh::{self, Net, Out, SshParams};
use crate::terminal::{self, Attached, Terminal};
use crate::Notify;
use anyhow::{anyhow, Context, Result};
use futures::executor::{LocalPool, LocalSpawner};
use futures::task::LocalSpawnExt;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thinkterm_font_web::{Face, FontSet};
use thinkterm_render::pipeline::{
    centred_projection, quad_indices, srgb_format, view_as, AtlasBindGroups, GpuTexture, Pipeline,
    ShaderUniform,
};
use thinkterm_render::vertex::{Vertex, IS_SOLID_COLOR};
use thinkterm_web::fallback::FallbackBudget;
use thinkterm_web::glyphs::GlyphCache;
use tokio::sync::mpsc::UnboundedSender;

pub struct ConnectParams {
    pub ssh: SshParams,
    pub font_paths: Vec<String>,
    pub size_pt: f64,
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
    /// From the network thread.
    Net(Net),
    /// The attach handshake finished (posted by its own future).
    Attached(Result<Attached, String>),
    /// The reattach handshake finished; `Err(true)` is permanent.
    Reattached(Result<(), (String, bool)>),
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
}

/// Device-level state: survives surfaces coming and going.
struct Gpu {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: Arc<wgpu::Queue>,
    pipeline: Option<(wgpu::TextureFormat, Pipeline)>,
    uniform_buffer: wgpu::Buffer,
    vertex_buffer: wgpu::Buffer,
    vertex_capacity: usize,
    index_buffer: wgpu::Buffer,
    index_quads: usize,
    /// One white texel for solid quads drawn without an atlas.
    white: Rc<GpuTexture>,
    atlas_groups: Vec<(usize, AtlasBindGroups)>,
    uniform_bind_group: Option<wgpu::BindGroup>,
    adapter_name: String,
}

/// The surface currently attached, if any.
struct Target {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    view_format: wgpu::TextureFormat,
    generation: u64,
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
    pushes: u64,
}

/// The connection, from the shell's point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Conn {
    Idle,
    Connecting,
    Attaching,
    Ready,
    /// Lost after being ready: trying to come back.
    Reconnecting,
    Disconnected(String),
}

const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(10);
const RECONNECT_GIVE_UP: u32 = 30;

struct State {
    cmd_tx: Sender<Cmd>,
    instance: wgpu::Instance,
    gpu: Option<Gpu>,
    target: Option<Target>,
    next_generation: u64,
    animating: bool,
    stats: Stats,
    notify: Arc<dyn Notify>,
    pool: LocalPool,
    spawner: LocalSpawner,
    clock: MobileClock,
    link: SshLink,
    host: Option<Arc<MobileHost>>,
    net_out: Option<UnboundedSender<Out>>,
    fonts: Option<Rc<FontSet>>,
    platform: Option<Rc<MobilePlatform>>,
    size_pt: f64,
    /// The glyph cache built for the handshake, handed to the terminal
    /// once the pane is known.
    pending_glyphs: Option<GlyphCache>,
    terminal: Option<Terminal>,
    conn: Conn,
    status: String,
    /// What to dial again when the transport drops.
    ssh_params: Option<SshParams>,
    reconnect_at: Option<Instant>,
    reconnect_delay: Duration,
    reconnect_attempts: u32,
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
    let spawner = pool.spawner();
    let notify: Arc<dyn Notify> = Arc::from(notify);
    let mut state = State {
        cmd_tx,
        instance,
        gpu: None,
        target: None,
        next_generation: 1,
        animating: false,
        stats: Stats::default(),
        notify,
        pool,
        spawner,
        clock: MobileClock::new(),
        link: SshLink::new(),
        host: None,
        net_out: None,
        fonts: None,
        platform: None,
        size_pt: 11.0,
        pending_glyphs: None,
        terminal: None,
        conn: Conn::Idle,
        status: String::new(),
        ssh_params: None,
        reconnect_at: None,
        reconnect_delay: RECONNECT_MIN,
        reconnect_attempts: 0,
    };
    state.notify.on_log("core thread up".into());
    state.set_status("idle");

    loop {
        // While animating with a surface and no pane, the demo changes its
        // scene every 100 ms and asks for a frame. Otherwise sleep until
        // told; the session's futures are all woken through commands.
        let mut wait = if state.animating && state.target.is_some() && state.terminal.is_none() {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(3600)
        };
        if let Some(at) = state.reconnect_at {
            wait = wait.min(at.saturating_duration_since(Instant::now()));
        }
        match rx.recv_timeout(wait) {
            Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(cmd) => state.handle(cmd),
            Err(RecvTimeoutError::Timeout) => {
                if state.reconnect_at.is_some_and(|at| Instant::now() >= at) {
                    state.reconnect_at = None;
                    state.try_reconnect();
                } else if state.terminal.is_none() {
                    state.stats.phase = state.stats.phase.wrapping_add(1);
                    state.request_frame();
                }
            }
        }
        state.pool.run_until_stalled();
        state.after_run();
    }
    state.disconnect();
    state.target = None;
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
                self.fit_terminal();
                self.try_attach_pane();
            }
            Cmd::Resize {
                generation,
                width,
                height,
                scale,
            } => {
                self.resize(generation, width, height, scale);
                self.fit_terminal();
            }
            Cmd::Detach { generation, reply } => {
                self.detach(generation);
                let _ = reply.send(());
            }
            Cmd::Render => {
                if let Err(err) = self.render() {
                    self.fail(format!("render: {err:#}"));
                }
            }
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
            Cmd::Net(net) => self.on_net(net),
            Cmd::Attached(outcome) => self.on_attached(outcome),
            Cmd::Reattached(outcome) => self.on_reattached(outcome),
            Cmd::Key {
                name,
                ctrl,
                alt,
                shift,
            } => {
                if let Some(terminal) = self.terminal.as_mut() {
                    if !terminal.key(&name, ctrl, alt, shift) {
                        self.notify.on_log(format!("key {name:?} has no mapping"));
                    }
                    self.request_frame();
                }
            }
            Cmd::Text(text) => {
                if let Some(terminal) = self.terminal.as_mut() {
                    terminal.text(&text);
                    self.request_frame();
                }
            }
            Cmd::Composing(on) => {
                if let Some(terminal) = self.terminal.as_mut() {
                    terminal.composing = on;
                }
            }
            Cmd::Paste(text) => {
                if let Some(terminal) = self.terminal.as_mut() {
                    terminal.paste(&text);
                    self.request_frame();
                }
            }
            Cmd::Scroll(lines) => {
                if let Some(terminal) = self.terminal.as_mut() {
                    terminal.scroll_lines(lines);
                    self.request_frame();
                }
            }
        }
    }

    /// After the executor ran: pushes the link collected while futures
    /// were answering, and dirty panes.
    fn after_run(&mut self) {
        let pushes = self.link.take_pushes();
        if !pushes.is_empty() {
            self.stats.pushes += pushes.len() as u64;
            let mut repaint = false;
            for pdu in pushes {
                if let Some(terminal) = self.terminal.as_mut() {
                    repaint |= terminal.on_push(pdu);
                }
            }
            // A push may have queued futures (render deltas): run them now
            // so the frame that follows sees their lines.
            self.pool.run_until_stalled();
            if repaint {
                self.request_frame();
            }
        }
        if let Some(host) = &self.host {
            if !host.events.take_dirty().is_empty() {
                self.request_frame();
            }
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
        self.notify.on_frame_needed();
    }

    // ----- connection -----

    fn connect(&mut self, params: ConnectParams, painter: Box<dyn GlyphPainter>) -> Result<()> {
        self.disconnect();
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
        self.platform = Some(Rc::new(MobilePlatform::new(painter, self.clock)));

        let host = Arc::new(MobileHost {
            clock: self.clock,
            spawner: Spawn(self.spawner.clone()),
            events: Events::default(),
            link: self.link.clone(),
            config: Config::default(),
        });
        let notify = Arc::clone(&self.notify);
        host.events
            .set_wake(Rc::new(move || notify.on_frame_needed()));
        self.host = Some(host);

        let tx = self.cmd_tx.clone();
        let deliver: Box<dyn Fn(Net) + Send> = Box::new(move |net| {
            let _ = tx.send(Cmd::Net(net));
        });
        self.net_out = Some(ssh::spawn(params.ssh.clone(), deliver));
        self.ssh_params = Some(params.ssh.clone());
        self.reconnect_attempts = 0;
        self.reconnect_delay = RECONNECT_MIN;
        self.reconnect_at = None;
        self.conn = Conn::Connecting;
        self.set_status(&format!(
            "connecting to {}@{}:{}",
            params.ssh.user, params.ssh.host, params.ssh.port
        ));
        Ok(())
    }

    fn disconnect(&mut self) {
        if let Some(out) = self.net_out.take() {
            let _ = out.send(Out::Close);
        }
        self.link.closed();
        self.terminal = None;
        self.pending_glyphs = None;
        self.ssh_params = None;
        self.reconnect_at = None;
        self.conn = Conn::Idle;
    }

    // ----- reconnecting -----

    /// The transport dropped under a live pane: keep the pane, dial again.
    fn lost(&mut self, reason: &str) {
        if let Some(terminal) = &self.terminal {
            terminal.session.set_dead(true);
        }
        self.conn = Conn::Reconnecting;
        self.schedule_reconnect(Duration::ZERO);
        self.set_status(&format!("connection lost ({reason}); reconnecting"));
        self.request_frame();
    }

    fn schedule_reconnect(&mut self, delay: Duration) {
        if self.reconnect_at.is_none() {
            self.reconnect_at = Some(Instant::now() + delay);
        }
    }

    fn try_reconnect(&mut self) {
        if self.conn != Conn::Reconnecting {
            return;
        }
        let Some(params) = self.ssh_params.clone() else {
            return;
        };
        if self.reconnect_attempts >= RECONNECT_GIVE_UP {
            self.conn = Conn::Disconnected("gave up reconnecting".into());
            self.set_status(&format!(
                "still disconnected after {} attempts; reconnect by hand",
                self.reconnect_attempts
            ));
            return;
        }
        self.reconnect_attempts += 1;
        self.notify
            .on_log(format!("reconnect attempt {}", self.reconnect_attempts));
        let tx = self.cmd_tx.clone();
        let deliver: Box<dyn Fn(Net) + Send> = Box::new(move |net| {
            let _ = tx.send(Cmd::Net(net));
        });
        self.net_out = Some(ssh::spawn(params, deliver));
    }

    fn reconnect_failed(&mut self, reason: &str, permanent: bool) {
        if permanent {
            self.conn = Conn::Disconnected(reason.to_string());
            self.set_status(&format!("not reconnecting: {reason}"));
            if let Some(out) = self.net_out.take() {
                let _ = out.send(Out::Close);
            }
            return;
        }
        self.reconnect_delay = (self.reconnect_delay * 2).clamp(RECONNECT_MIN, RECONNECT_MAX);
        let delay = self.reconnect_delay;
        self.notify.on_log(format!(
            "reconnect failed ({reason}); retrying in {delay:?}"
        ));
        self.conn = Conn::Reconnecting;
        self.schedule_reconnect(delay);
    }

    /// The transport is up again: run the reattach handshake on it.
    fn start_reattach(&mut self) {
        let Some(terminal) = self.terminal.as_ref() else {
            return;
        };
        let size = self.target.as_ref().map(|_| terminal.size());
        let (tab_id, pane_id) = (terminal.tab_id, terminal.pane_id);
        let link = self.link.clone();
        let tx = self.cmd_tx.clone();
        let spawned = self.spawner.spawn_local(async move {
            let outcome = terminal::reattach(&link, tab_id, pane_id, size)
                .await
                .map(|_| ())
                .map_err(|err| {
                    let permanent = err.downcast_ref::<terminal::NoPanes>().is_some()
                        || err.to_string().contains("update the server or the app");
                    (format!("{err:#}"), permanent)
                });
            let _ = tx.send(Cmd::Reattached(outcome));
        });
        if let Err(err) = spawned {
            self.fail(format!("spawning the reattach: {err}"));
        }
    }

    fn on_reattached(&mut self, outcome: Result<(), (String, bool)>) {
        match outcome {
            Ok(()) => {
                self.reconnect_attempts = 0;
                self.reconnect_delay = RECONNECT_MIN;
                let pane = self.terminal.as_mut().map(|terminal| {
                    terminal.reconnected();
                    terminal.pane_id
                });
                if let Some(pane) = pane {
                    self.conn = Conn::Ready;
                    self.set_status(&format!("pane {pane} · reconnected"));
                }
                self.request_frame();
            }
            Err((reason, permanent)) => self.reconnect_failed(&reason, permanent),
        }
    }

    fn on_net(&mut self, net: Net) {
        match net {
            Net::Connected => {
                if let Some(out) = self.net_out.clone() {
                    self.link.opened(out);
                }
                if self.conn == Conn::Reconnecting && self.terminal.is_some() {
                    self.set_status("reconnected; reattaching");
                    self.start_reattach();
                } else {
                    self.conn = Conn::Attaching;
                    self.set_status("connected; attaching");
                    self.try_attach_pane();
                }
            }
            Net::Data(bytes) => {
                self.stats.bytes_in += bytes.len() as u64;
                if let Err(err) = self.link.feed(&bytes) {
                    self.fail(format!("link: {err:#}"));
                    self.conn = Conn::Disconnected(format!("{err:#}"));
                    self.set_status(&format!("disconnected: {err:#}"));
                }
            }
            Net::Stderr(text) => self.notify.on_log(format!("remote: {}", text.trim_end())),
            Net::Exit(code) => self
                .notify
                .on_log(format!("remote command exited with {code}")),
            Net::Closed(reason) => {
                self.link.closed();
                self.net_out = None;
                self.pending_glyphs = None;
                match self.conn {
                    Conn::Ready => self.lost(&reason),
                    Conn::Reconnecting => self.reconnect_failed(&reason, false),
                    _ => {
                        self.conn = Conn::Disconnected(reason.clone());
                        self.set_status(&format!("disconnected: {reason}"));
                        self.request_frame();
                    }
                }
            }
        }
    }

    /// Start the handshake once the link is open and the GPU and fonts
    /// exist (the glyph metrics decide the grid to report).
    fn try_attach_pane(&mut self) {
        if self.conn != Conn::Attaching || self.pending_glyphs.is_some() || self.terminal.is_some()
        {
            return;
        }
        if !self.link.is_open() {
            return;
        }
        let (Some(gpu), Some(fonts), Some(platform)) = (&self.gpu, &self.fonts, &self.platform)
        else {
            return;
        };
        let scale = self.target.as_ref().map(|t| t.scale).unwrap_or(1.0);
        // `size_pt` is in the platform's points (1/72 in on iOS), so the
        // dpi is 72 per unit of scale -- not the browser's 96, which would
        // make 11 pt a 44 px face on a 3x screen.
        let dpi = (72.0 * scale) as u32;
        let side = 1024u32.min(gpu.device.limits().max_texture_dimension_2d);
        let texture = match GpuTexture::new(&gpu.device, Arc::clone(&gpu.queue), side, side) {
            Ok(t) => Rc::new(t),
            Err(err) => {
                self.fail(format!("atlas: {err:#}"));
                return;
            }
        };
        let glyphs = match GlyphCache::new(
            Rc::clone(fonts),
            self.size_pt,
            dpi,
            texture,
            Rc::from(""),
            Rc::clone(platform) as thinkterm_web::raster::Platform,
        ) {
            Ok(g) => g,
            Err(err) => {
                self.fail(format!("glyph cache: {err:#}"));
                return;
            }
        };
        let (cw, ch) = (
            glyphs.metrics.cell_size.width as u32,
            glyphs.metrics.cell_size.height as u32,
        );
        let size = self.target.as_ref().and_then(|t| {
            terminal::grid_for(t.config.width, t.config.height, cw, ch).map(|(cols, rows)| {
                wezterm_term::TerminalSize {
                    rows,
                    cols,
                    pixel_width: cols * cw as usize,
                    pixel_height: rows * ch as usize,
                    dpi,
                }
            })
        });
        self.pending_glyphs = Some(glyphs);
        self.notify.on_log(format!(
            "cell {cw}x{ch} px at {} pt, {dpi} dpi; reporting {:?}",
            self.size_pt,
            size.map(|s| (s.cols, s.rows))
        ));
        let link = self.link.clone();
        let tx = self.cmd_tx.clone();
        let wall = thinkterm_session::clock::Clock::wall_millis(&self.clock);
        let spawned = self.spawner.spawn_local(async move {
            let outcome = terminal::attach(&link, size, wall)
                .await
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Cmd::Attached(outcome));
        });
        if let Err(err) = spawned {
            self.fail(format!("spawning the handshake: {err}"));
        }
    }

    fn on_attached(&mut self, outcome: Result<Attached, String>) {
        let Some(glyphs) = self.pending_glyphs.take() else {
            return;
        };
        match outcome {
            Ok(attached) => {
                let Some(host) = self.host.clone() else {
                    return;
                };
                let (cw, ch) = (
                    glyphs.metrics.cell_size.width as u32,
                    glyphs.metrics.cell_size.height as u32,
                );
                let (cols, rows) = self
                    .target
                    .as_ref()
                    .and_then(|t| terminal::grid_for(t.config.width, t.config.height, cw, ch))
                    .unwrap_or((attached.dims.cols, attached.dims.viewport_rows));
                let dpi = glyphs.dpi;
                self.notify.on_log(format!(
                    "attached to pane {} in tab {} on {} ({cols}x{rows}), server {}",
                    attached.pane_id, attached.tab_id, attached.server_version, attached.server_id
                ));
                self.terminal = Some(Terminal::new(host, glyphs, &attached, cols, rows, dpi));
                self.conn = Conn::Ready;
                self.set_status(&format!("pane {} · {}", attached.pane_id, attached.title));
                self.request_frame();
            }
            Err(err) => {
                self.fail(format!("attach: {err}"));
                self.conn = Conn::Disconnected(err.clone());
                self.set_status(&format!("attach failed: {err}"));
                if let Some(out) = self.net_out.take() {
                    let _ = out.send(Out::Close);
                }
            }
        }
    }

    /// The grid follows the surface.
    fn fit_terminal(&mut self) {
        let Some(target) = self.target.as_ref() else {
            return;
        };
        let (w, h) = (target.config.width, target.config.height);
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        let (cw, ch) = terminal.cell_size();
        if let Some((cols, rows)) = terminal::grid_for(w, h, cw, ch) {
            if terminal.resize(cols, rows) {
                self.request_frame();
            }
        }
    }

    // ----- surface -----

    fn attach(&mut self, layer: usize, width: u32, height: u32, scale: f64) -> Result<u64> {
        if self.target.is_some() {
            anyhow::bail!("a surface is already attached; detach it first");
        }
        let surface = self.surface_from_layer(layer)?;
        if self.gpu.is_none() {
            self.gpu = Some(Self::make_gpu(&self.instance, &surface)?);
            let name = self.gpu.as_ref().unwrap().adapter_name.clone();
            self.notify.on_log(format!("gpu ready: {name}"));
        }
        let gpu = self.gpu.as_mut().unwrap();
        let caps = surface.get_capabilities(&gpu.adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| matches!(f, wgpu::TextureFormat::Bgra8Unorm))
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| anyhow!("the surface offers no texture format"))?;
        let view_format = srgb_format(format);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![view_format],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&gpu.device, &config);
        gpu.ensure_pipeline(view_format);

        let generation = self.next_generation;
        self.next_generation += 1;
        self.target = Some(Target {
            surface,
            config,
            view_format,
            generation,
            scale,
        });
        self.stats.attaches += 1;
        self.stats.generation = generation;
        self.stats.width = width;
        self.stats.height = height;
        self.stats.scale = scale;
        self.notify.on_log(format!(
            "attached generation {generation}: {width}x{height} @{scale} {format:?}"
        ));
        self.request_frame();
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

    #[cfg(not(target_vendor = "apple"))]
    fn surface_from_layer(&self, _layer: usize) -> Result<wgpu::Surface<'static>> {
        anyhow::bail!("no surface source on this platform yet")
    }

    fn make_gpu(instance: &wgpu::Instance, surface: &wgpu::Surface<'static>) -> Result<Gpu> {
        let adapter =
            futures::executor::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(surface),
                force_fallback_adapter: false,
            }))
            .context("requesting a GPU adapter")?;
        let (device, queue) =
            futures::executor::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("thinkterm-mobile"),
                required_features: wgpu::Features::empty(),
                required_limits:
                    wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            }))
            .context("requesting the GPU device")?;
        let queue = Arc::new(queue);
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ShaderUniform"),
            size: std::mem::size_of::<ShaderUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let vertex_capacity = 4096;
        let vertex_buffer = Self::make_vertex_buffer(&device, vertex_capacity);
        let index_quads = 1024;
        let index_buffer = Self::make_index_buffer(&device, &queue, index_quads);
        let white = GpuTexture::new(&device, Arc::clone(&queue), 1, 1)?;
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: white.texture(),
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255, 255, 255, 255],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let adapter_name = {
            let info = adapter.get_info();
            format!("{} ({:?})", info.name, info.backend)
        };
        Ok(Gpu {
            adapter,
            device,
            queue,
            pipeline: None,
            uniform_buffer,
            vertex_buffer,
            vertex_capacity,
            index_buffer,
            index_quads,
            white: Rc::new(white),
            atlas_groups: Vec::new(),
            uniform_bind_group: None,
            adapter_name,
        })
    }

    fn make_vertex_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vertices"),
            size: (capacity * std::mem::size_of::<Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn make_index_buffer(device: &wgpu::Device, queue: &wgpu::Queue, quads: usize) -> wgpu::Buffer {
        let indices = quad_indices(quads);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("indices"),
            size: (indices.len() * std::mem::size_of::<u32>()) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&buffer, 0, bytemuck::cast_slice(&indices));
        buffer
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
        target.scale = scale;
        self.stats.scale = scale;
        if (width, height) == (target.config.width, target.config.height) {
            return;
        }
        target.config.width = width;
        target.config.height = height;
        let gpu = self.gpu.as_ref().expect("gpu exists while a target does");
        target.surface.configure(&gpu.device, &target.config);
        self.stats.resizes += 1;
        self.stats.width = width;
        self.stats.height = height;
        self.request_frame();
    }

    fn detach(&mut self, generation: u64) {
        match self.target.take() {
            Some(target) if target.generation == generation => {
                drop(target);
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

    fn render(&mut self) -> Result<()> {
        let Some(target) = self.target.as_mut() else {
            // A frame requested before the surface came, or after it went:
            // nothing to draw on, and nothing to do.
            return Ok(());
        };
        let gpu = self.gpu.as_mut().expect("gpu exists while a target does");
        let started = Instant::now();
        let (w, h) = (target.config.width, target.config.height);

        let (vertices, texture, background): (Vec<Vertex>, Rc<GpuTexture>, [f32; 4]) =
            match (self.terminal.as_mut(), self.platform.as_ref()) {
                (Some(terminal), Some(platform)) => {
                    let mut budget = FallbackBudget::new(
                        thinkterm_web::raster::GlyphPlatform::now_ms(&**platform),
                    );
                    let (vertices, bg) = {
                        let (vertices, _, bg) = terminal.paint((w, h), &mut budget)?;
                        (vertices.to_vec(), bg.tuple())
                    };
                    // Glyphs the budget put off are owed to the next frame.
                    let owed = budget.deferred() + terminal.glyphs.declined();
                    if owed > 0 {
                        self.stats.frames_requested += 1;
                        self.notify.on_frame_needed();
                    }
                    (
                        vertices,
                        terminal.glyphs.texture_rc(),
                        [bg.0, bg.1, bg.2, bg.3],
                    )
                }
                _ => (
                    demo_scene(w as f32, h as f32, target.scale as f32, self.stats.phase),
                    Rc::clone(&gpu.white),
                    [0.07, 0.07, 0.09, 1.0],
                ),
            };

        let uniforms = ShaderUniform {
            foreground_text_hsb: [1.0, 1.0, 1.0],
            milliseconds: (started.elapsed().as_millis() % u32::MAX as u128) as u32,
            viewport_and_corner: [w as f32, h as f32, 0.0, 0.0],
            window_border: [0.0; 4],
            projection: centred_projection(w as f32, h as f32),
        };
        gpu.queue
            .write_buffer(&gpu.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));
        let quads = gpu.upload_vertices(&vertices);
        let group = gpu.atlas_groups(&texture);

        let frame = match target.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost) | Err(wgpu::SurfaceError::Outdated) => {
                target.surface.configure(&gpu.device, &target.config);
                target
                    .surface
                    .get_current_texture()
                    .context("acquiring the frame after reconfiguring")?
            }
            Err(wgpu::SurfaceError::Timeout) => {
                self.notify.on_log("frame skipped: surface timeout".into());
                return Ok(());
            }
            Err(err) => return Err(anyhow!("acquiring the frame: {err}")),
        };
        let view = view_as(&frame.texture, target.view_format);
        let pipeline = &gpu.pipeline.as_ref().expect("pipeline built at attach").1;
        let groups = &gpu.atlas_groups[group].1;
        let uniform_group = gpu
            .uniform_bind_group
            .as_ref()
            .expect("uniform group built at attach");
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pane"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: background[0] as f64,
                            g: background[1] as f64,
                            b: background[2] as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if quads > 0 {
                pass.set_pipeline(&pipeline.render_pipeline);
                pass.set_bind_group(0, uniform_group, &[]);
                pass.set_bind_group(1, &groups.linear, &[]);
                pass.set_bind_group(2, &groups.nearest, &[]);
                pass.set_vertex_buffer(0, gpu.vertex_buffer.slice(..));
                pass.set_index_buffer(gpu.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..(quads * 6) as u32, 0, 0..1);
            }
        }
        gpu.queue.submit(Some(encoder.finish()));
        frame.present();
        self.stats.frames += 1;
        self.stats.last_frame_us = started.elapsed().as_micros();
        Ok(())
    }

    fn stats_json(&self) -> String {
        let s = &self.stats;
        let adapter = self
            .gpu
            .as_ref()
            .map(|g| g.adapter_name.clone())
            .unwrap_or_default();
        let grid = self
            .terminal
            .as_ref()
            .map(|t| format!("{}x{}", t.cols, t.rows))
            .unwrap_or_default();
        let (inputs, composing) = self
            .terminal
            .as_ref()
            .map(|t| (t.inputs_sent, t.composing))
            .unwrap_or((0, false));
        format!(
            "{{\"attached\":{},\"generation\":{},\"size\":\"{}x{}@{}\",\"attaches\":{},\"detaches\":{},\"resizes\":{},\"frames\":{},\"frames_requested\":{},\"last_frame_us\":{},\"phase\":{},\"animating\":{},\"errors\":{},\"last_error\":{:?},\"adapter\":{:?},\"conn\":{:?},\"grid\":{:?},\"bytes_in\":{},\"pushes\":{},\"inputs_sent\":{},\"composing\":{}}}",
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
            adapter,
            format!("{:?}", self.conn),
            grid,
            s.bytes_in,
            s.pushes,
            inputs,
            composing,
        )
    }
}

impl Gpu {
    fn ensure_pipeline(&mut self, view_format: wgpu::TextureFormat) {
        let stale = self
            .pipeline
            .as_ref()
            .map(|(format, _)| *format != view_format)
            .unwrap_or(true);
        if stale {
            let pipeline = Pipeline::new(&self.device, view_format);
            self.uniform_bind_group =
                Some(pipeline.uniform_bind_group(&self.device, &self.uniform_buffer));
            self.pipeline = Some((view_format, pipeline));
            self.atlas_groups.clear();
        }
    }

    /// Bind groups for `atlas`, made on first sight and kept for a few.
    fn atlas_groups(&mut self, atlas: &GpuTexture) -> usize {
        let identity = atlas.id();
        if let Some(i) = self.atlas_groups.iter().position(|(id, _)| *id == identity) {
            return i;
        }
        if self.atlas_groups.len() >= 8 {
            self.atlas_groups.remove(0);
        }
        let pipeline = &self.pipeline.as_ref().expect("pipeline built at attach").1;
        let groups = pipeline.atlas_bind_groups(&self.device, &atlas.view());
        self.atlas_groups.push((identity, groups));
        self.atlas_groups.len() - 1
    }

    fn upload_vertices(&mut self, vertices: &[Vertex]) -> usize {
        let quads = vertices.len() / 4;
        if vertices.len() > self.vertex_capacity {
            self.vertex_capacity = vertices.len().next_power_of_two();
            self.vertex_buffer = State::make_vertex_buffer(&self.device, self.vertex_capacity);
        }
        if quads > self.index_quads {
            self.index_quads = quads.next_power_of_two();
            self.index_buffer =
                State::make_index_buffer(&self.device, &self.queue, self.index_quads);
        }
        if !vertices.is_empty() {
            self.queue
                .write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(vertices));
        }
        quads
    }
}

/// A grid of coloured cells the size a terminal's would be, one of them
/// walking across with the phase, and a bar along the bottom whose length
/// follows the width. Enough to see, in a screenshot, that the projection,
/// the scale and a resize all land where they should.
fn demo_scene(width: f32, height: f32, scale: f32, phase: u64) -> Vec<Vertex> {
    let mut out = Vec::new();
    let cell_w = 9.0 * scale;
    let cell_h = 20.0 * scale;
    let cols = ((width - 2.0 * cell_w) / cell_w).floor().max(1.0) as u64;
    let rows = ((height - 4.0 * cell_h) / cell_h).floor().max(1.0) as u64;
    let x0 = cell_w;
    let y0 = cell_h * 2.0;
    for row in 0..rows {
        for col in 0..cols {
            let i = row * cols + col;
            let lit = (i + phase) % 7 == 0;
            let color = if lit {
                let hue = ((i * 37 + phase * 5) % 360) as f32;
                hsl(hue, 0.7, 0.55)
            } else {
                [0.16, 0.16, 0.20, 1.0]
            };
            push_rect(
                &mut out,
                width,
                height,
                x0 + col as f32 * cell_w,
                y0 + row as f32 * cell_h,
                cell_w - 1.0 * scale,
                cell_h - 1.0 * scale,
                color,
            );
        }
    }
    let walker = (phase % cols.max(1)) as f32;
    push_rect(
        &mut out,
        width,
        height,
        x0 + walker * cell_w,
        cell_h * 0.5,
        cell_w,
        cell_h,
        [1.0, 0.85, 0.2, 1.0],
    );
    push_rect(
        &mut out,
        width,
        height,
        x0,
        height - cell_h * 1.5,
        width - 2.0 * x0,
        cell_h * 0.5,
        [0.3, 0.7, 1.0, 1.0],
    );
    out
}

#[allow(clippy::too_many_arguments)]
fn push_rect(
    out: &mut Vec<Vertex>,
    width: f32,
    height: f32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: [f32; 4],
) {
    // The projection is centred: pixel (0,0) is at (-width/2, -height/2).
    let left = x - width / 2.0;
    let top = y - height / 2.0;
    let right = left + w;
    let bottom = top + h;
    let vertex = |px: f32, py: f32| Vertex {
        position: [px, py],
        tex: [0.0, 0.0],
        fg_color: color,
        alt_color: color,
        hsv: [1.0, 1.0, 1.0],
        has_color: IS_SOLID_COLOR,
        mix_value: 0.0,
    };
    // Order matches thinkterm_render::vertex::{V_TOP_LEFT, V_TOP_RIGHT,
    // V_BOT_LEFT, V_BOT_RIGHT}, which quad_indices assumes.
    out.push(vertex(left, top));
    out.push(vertex(right, top));
    out.push(vertex(left, bottom));
    out.push(vertex(right, bottom));
}

fn hsl(h: f32, s: f32, l: f32) -> [f32; 4] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    [r + m, g + m, b + m, 1.0]
}
