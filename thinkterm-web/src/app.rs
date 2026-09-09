//! The page's state: one pane session, the canvas it is drawn on, where
//! the user is looking, what they selected, and who owns the tab.

use crate::fallback::{Capacity, FallbackBudget, Next, MIN_RETRY_MS};
use crate::glyphs::GlyphCache;
use crate::gpu::Gpu;
use crate::host::WebHost;
use crate::link::WsLink;
use crate::viewport::{cell_at, max_scroll, visible_rows};
use anyhow::Result;
use codec::Pdu;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use thinkterm_session::host::SessionEvents as _;
use termwiz::input::{KeyCode, Modifiers};
use thinkterm_font_web::FontSet;
use thinkterm_proto::{PaneId, TabId};
use thinkterm_render::atlas::OutOfTextureSpace;
use thinkterm_render::pipeline::GpuTexture;
use thinkterm_render::quad::HeapQuadAllocator;
use thinkterm_render::vertex::Vertex;
use thinkterm_session::pane::PaneSession;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wezterm_term::color::ColorPalette;
use wezterm_term::input::{MouseButton, MouseEvent, MouseEventKind};
use wezterm_term::{KeyModifiers, StableRowIndex, TerminalSize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pointer {
    Down,
    Move,
    Up,
}

#[derive(Debug, Clone, Copy)]
struct Selection {
    anchor: (StableRowIndex, usize),
    head: (StableRowIndex, usize),
    /// 1 = cells, 2 = words, 3 = lines.
    mode: u8,
}

impl Selection {
    fn ordered(&self) -> ((StableRowIndex, usize), (StableRowIndex, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// Everything the page hands the app once it is attached.
/// Reconnect backoff. The first attempt is immediate -- a socket that
/// dropped because a laptop's wifi blinked is usually back at once -- and
/// the wait doubles from there so a server that is down for an hour is not
/// hammered.
const RECONNECT_MIN_MS: f64 = 500.0;
const RECONNECT_MAX_MS: f64 = 15_000.0;
/// After this many failed attempts the status line stops saying
/// "reconnecting" and admits it may never work.
const RECONNECT_DOUBT_AFTER: u32 = 6;
/// How long a new connection has to last before the backoff is forgiven.
/// Shorter than the shortest useful outage and longer than the time a
/// server that is restarting in a loop stays up.
const RECONNECT_STABLE_MS: f64 = 5_000.0;

pub struct Setup {
    pub link: WsLink,
    pub session: Arc<PaneSession<WebHost>>,
    pub host: Arc<WebHost>,
    pub gpu: Gpu,
    pub glyphs: GlyphCache,
    pub fonts: Rc<FontSet>,
    pub canvas: web_sys::HtmlCanvasElement,
    pub textarea: web_sys::HtmlTextAreaElement,
    pub status: Option<web_sys::Element>,
    /// Kept so the page can reopen the socket by itself.
    pub url: String,
    pub token: String,
    pub pane_id: PaneId,
    pub tab_id: TabId,
    pub dpr: f64,
    pub cols: usize,
    pub rows: usize,
    pub title: String,
    pub strip: Option<crate::chrome::TabStrip>,
}

pub struct Inner {
    link: WsLink,
    session: Arc<PaneSession<WebHost>>,
    host: Arc<WebHost>,
    gpu: Gpu,
    glyphs: GlyphCache,
    fonts: Rc<FontSet>,
    canvas: web_sys::HtmlCanvasElement,
    textarea: web_sys::HtmlTextAreaElement,
    status: Option<web_sys::Element>,
    palette: ColorPalette,
    pane_id: PaneId,
    tab_id: TabId,
    dpr: f64,
    cols: usize,
    rows: usize,
    scroll_from_bottom: usize,
    selection: Option<Selection>,
    /// Word/line selection extends by units; the pointer is down.
    selecting: bool,
    click_count: u8,
    last_click_ms: f64,
    focused: bool,
    composing: bool,
    /// Refreshed on layout/scroll changes, avoiding layout reads per glyph
    /// or per frame. The IME field itself uses fixed CSS positioning.
    canvas_rect: [f64; 4],
    ime_anchor: Option<crate::ime::Anchor>,
    /// Set while there is no live socket. Cleared by a reconnect, which is
    /// why it is no longer the end of the page's life.
    disconnected: Option<String>,
    url: String,
    token: String,
    /// How long to wait before the next attempt, and whether one is already
    /// scheduled. Attempts are counted only to change what the status line
    /// says after enough of them.
    reconnect_delay: f64,
    reconnect_pending: bool,
    reconnect_attempts: u32,
    /// When the current connection came up, until it has lasted long enough
    /// to be called good. `None` once the backoff has been forgiven.
    connected_since: Option<f64>,
    quads: HeapQuadAllocator,
    vertices: Vec<Vertex>,
    title: String,
    /// What to do about an atlas that has run out of room for good.
    capacity: Capacity,
    /// The server's tabs and panes as last listed, for the strip and for
    /// switching. Refreshed on the pushes that change it and on a timer.
    layout: Option<codec::ListPanesResponse>,
    /// Whether the page moves to whatever pane the desktop focuses. On by
    /// default: a page opened to "see my terminal" wants the one being
    /// used. Choosing a pane here turns it off.
    following: bool,
    strip: Option<crate::chrome::TabStrip>,
    layout_refresh_pending: bool,
    /// A switch is under way; a second request waits for the next push
    /// or click rather than racing it.
    switching: bool,
}

pub struct App {
    inner: RefCell<Inner>,
    frame_requested: Cell<bool>,
    raf: RefCell<Option<Closure<dyn FnMut()>>>,
    /// The atlas backoff's own wake-up. Everything else here is driven by
    /// input or by output; this is the one thing that has to happen on a
    /// still screen.
    retry: RefCell<Option<Closure<dyn FnMut()>>>,
    retry_pending: Cell<bool>,
    /// When the pending timer is due, so a nearer one can replace it.
    retry_due: Cell<f64>,
}

/// How large the glyph atlas is allowed to get.
///
/// Not the GPU's maximum, which is commonly 8192: that is 268 MB of video
/// memory for glyphs alone, which is not a trade a terminal should make
/// silently. At this size it holds several screens' worth of distinct CJK,
/// and beyond it `Capacity` clears and reuses rather than growing -- which
/// is the whole reason that path exists.
const MAX_ATLAS_SIDE: usize = 4096;

/// What `grow_atlas` managed.
enum Grown {
    /// A bigger texture; try again.
    Larger,
    /// Already at the GPU's largest, and it is full. Only a clear frees
    /// space now, and `Capacity` decides when that is worth doing.
    AtCapacity,
}

fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// Milliseconds from a clock that does not step.
///
/// `Date::now` is wall time, and the atlas backoff stores an absolute
/// deadline: an NTP correction or a VM host resync that moves the clock
/// backwards would park the terminal in its degraded mode for as long as
/// the correction, with every frame answering "not yet".
pub(crate) fn monotonic_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or_else(now_ms)
}

/// A session for one pane, configured the way this page runs them.
pub fn build_session(
    host: &Arc<WebHost>,
    pane_id: PaneId,
    tab_id: TabId,
    dims: thinkterm_proto::RenderableDimensions,
    title: &str,
    alt_screen: bool,
) -> Arc<PaneSession<WebHost>> {
    PaneSession::new(
        Arc::clone(host),
        Arc::new(thinkterm_session::Lock::new(
            thinkterm_session::images::ImageStore::default(),
        )),
        thinkterm_session::SessionConfig {
            scrollback_lines: 3500,
            local_echo_threshold_ms: Some(100),
            overlay_lag_indicator: false,
        },
        pane_id,
        Arc::new(std::sync::atomic::AtomicUsize::new(tab_id)),
        0,
        dims,
        title,
        alt_screen,
    )
}

impl App {
    pub fn new(setup: Setup) -> Rc<Self> {
        let inner = Inner {
            link: setup.link,
            session: setup.session,
            host: setup.host,
            gpu: setup.gpu,
            glyphs: setup.glyphs,
            fonts: setup.fonts,
            canvas: setup.canvas,
            textarea: setup.textarea,
            status: setup.status,
            palette: ColorPalette::default(),
            pane_id: setup.pane_id,
            tab_id: setup.tab_id,
            dpr: setup.dpr,
            cols: setup.cols,
            rows: setup.rows,
            scroll_from_bottom: 0,
            selection: None,
            selecting: false,
            click_count: 0,
            last_click_ms: 0.0,
            focused: true,
            composing: false,
            canvas_rect: [0.0; 4],
            ime_anchor: None,
            disconnected: None,
            url: setup.url,
            token: setup.token,
            reconnect_delay: RECONNECT_MIN_MS,
            reconnect_pending: false,
            reconnect_attempts: 0,
            connected_since: None,
            capacity: Capacity::new(),
            quads: HeapQuadAllocator::default(),
            vertices: Vec::new(),
            title: setup.title,
            layout: None,
            following: true,
            strip: setup.strip,
            layout_refresh_pending: false,
            switching: false,
        };
        let app = Rc::new(Self {
            inner: RefCell::new(inner),
            frame_requested: Cell::new(false),
            raf: RefCell::new(None),
            retry: RefCell::new(None),
            retry_pending: Cell::new(false),
            retry_due: Cell::new(0.0),
        });
        let weak = Rc::downgrade(&app);
        *app.raf.borrow_mut() = Some(Closure::<dyn FnMut()>::new(move || {
            if let Some(app) = weak.upgrade() {
                app.frame_requested.set(false);
                app.frame();
            }
        }));
        let weak = Rc::downgrade(&app);
        *app.retry.borrow_mut() = Some(Closure::<dyn FnMut()>::new(move || {
            if let Some(app) = weak.upgrade() {
                app.retry_pending.set(false);
                app.request_frame();
            }
        }));
        app
    }

    /// Come back after `delay_ms` whether or not anything else asks for a
    /// frame.
    ///
    /// The atlas backoff is the only thing here that cannot wait to be
    /// asked: once the terminal goes quiet nothing requests another frame,
    /// so a retry that counted frames would never arrive -- and repainting a
    /// still screen thousands of times to reach a count would be waste.
    fn schedule_retry(&self, delay_ms: f64) {
        // A timer already set for sooner will do. One set for later will
        // not: dropping a nearer deadline on the floor is how a recovery
        // that should have taken a second takes eight.
        let due = monotonic_ms() + delay_ms;
        if self.retry_pending.get() && self.retry_due.get() <= due {
            return;
        }
        let retry = self.retry.borrow();
        if let (Some(window), Some(closure)) = (web_sys::window(), retry.as_ref()) {
            if window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    closure.as_ref().unchecked_ref(),
                    delay_ms.clamp(0.0, i32::MAX as f64) as i32,
                )
                .is_ok()
            {
                self.retry_pending.set(true);
                self.retry_due.set(due);
            }
        }
    }

    pub fn wake(self: &Rc<Self>) -> Rc<dyn Fn()> {
        let weak = Rc::downgrade(self);
        Rc::new(move || {
            if let Some(app) = weak.upgrade() {
                app.request_frame();
            }
        })
    }

    pub fn request_frame(&self) {
        if self.frame_requested.get() {
            return;
        }
        let raf = self.raf.borrow();
        if let (Some(window), Some(closure)) = (web_sys::window(), raf.as_ref()) {
            if window
                .request_animation_frame(closure.as_ref().unchecked_ref())
                .is_ok()
            {
                self.frame_requested.set(true);
            }
        }
    }

    fn set_status(inner: &Inner, text: &str) {
        if let Some(el) = &inner.status {
            el.set_text_content(Some(text));
        }
    }

    /// The server pushed something for us.
    pub fn on_push(self: &Rc<Self>, pdu: Pdu) {
        let inner = self.inner.borrow();
        match pdu {
            Pdu::GetPaneRenderChangesResponse(delta) if delta.pane_id == inner.pane_id => {
                inner.session.queue_render_delta(delta);
            }
            Pdu::PaneRemoved(removed) if removed.pane_id == inner.pane_id => {
                inner.session.set_dead(true);
                Self::set_status(&inner, "the pane was closed on the server");
                // Something else to show, if the server has anything.
                drop(inner);
                self.refresh_layout();
            }
            // The desktop moved. Followed only while following; a page
            // that chose a pane is not dragged off it.
            Pdu::PaneFocused(focused) if inner.following && focused.pane_id != inner.pane_id => {
                drop(inner);
                self.switch_to_pane(focused.pane_id, false);
            }
            // The strip is stale: a tab came or went, a pane closed
            // elsewhere, a title changed. Listed again, a moment later,
            // so a burst of these is one request.
            Pdu::PaneRemoved(_)
            | Pdu::TabAddedToWindow(_)
            | Pdu::TabTitleChanged(_)
            | Pdu::WindowTitleChanged(_)
            | Pdu::TabResized(_)
            | Pdu::WindowWorkspaceChanged(_) => {
                drop(inner);
                self.refresh_layout_soon();
            }
            // Sent once per connection, before the first line change: a
            // program's OSC 4/10/11 colours. Dropping it would leave the
            // page on the stock palette for good.
            Pdu::SetApplicationPalette(codec::SetApplicationPalette { pane_id, palette })
                if pane_id == inner.pane_id =>
            {
                drop(inner);
                let mut inner = self.inner.borrow_mut();
                let palette = palette.unwrap_or_default();
                if palette != inner.palette {
                    inner.palette = palette;
                    inner.session.make_all_stale();
                    drop(inner);
                    self.request_frame();
                }
            }
            Pdu::ClientViewportState(_) | Pdu::FrontendAccessState(_) => {
                drop(inner);
                self.refresh_status();
                self.request_frame();
            }
            Pdu::SetClipboard(clip) => {
                if let Some(text) = clip.clipboard {
                    write_clipboard(&text);
                }
            }
            _ => {}
        }
    }

    pub fn on_close(self: &Rc<Self>, reason: String) {
        {
            let mut inner = self.inner.borrow_mut();
            if inner.disconnected.is_some() {
                return;
            }
            inner.disconnected = Some(reason.clone());
            // The connection that just died settles its own backoff here:
            // one that held for a while earns the short delay back, one
            // that did not keeps the long one. Left set, a frame drawn
            // during the outage would forgive a connection that is gone.
            if let Some(at) = inner.connected_since.take() {
                if monotonic_ms() - at >= RECONNECT_STABLE_MS {
                    inner.reconnect_delay = RECONNECT_MIN_MS;
                }
            }
            // Nothing more will arrive on this socket: the watchdog must
            // stop asking, or the page keeps requesting lines that never
            // come. `reconnected` turns it back on.
            inner.session.set_dead(true);
            log::warn!("connection lost: {reason}");
        }
        self.show_reconnect_status();
        self.schedule_reconnect(0.0);
    }

    /// What the status line says while there is no connection.
    ///
    /// The reason is shown as well as the count, because the page cannot
    /// tell the interesting cases apart: a browser is not told why an
    /// upgrade was refused, so a revoked link, an expired one and a server
    /// that is simply not running all arrive here as the same closed
    /// socket. After a while the message says so rather than counting up
    /// for ever in silence.
    fn show_reconnect_status(&self) {
        let inner = self.inner.borrow();
        let Some(reason) = inner.disconnected.clone() else {
            return;
        };
        let text = if inner.reconnect_attempts >= RECONNECT_DOUBT_AFTER {
            format!(
                "still trying to reconnect after {} attempts ({reason}). \
                 The link may have expired, or the server may be down.",
                inner.reconnect_attempts
            )
        } else {
            format!("connection lost ({reason}); reconnecting…")
        };
        Self::set_status(&inner, &text);
    }

    /// Come back and try again. `0.0` means as soon as the browser will.
    fn schedule_reconnect(self: &Rc<Self>, delay_ms: f64) {
        {
            let mut inner = self.inner.borrow_mut();
            if inner.reconnect_pending || inner.disconnected.is_none() {
                return;
            }
            inner.reconnect_pending = true;
        }
        let app = Rc::clone(self);
        let closure = Closure::once_into_js(move || {
            app.inner.borrow_mut().reconnect_pending = false;
            wasm_bindgen_futures::spawn_local(app.try_reconnect());
        });
        let armed = web_sys::window().is_some_and(|window| {
            window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    closure.as_ref().unchecked_ref(),
                    delay_ms.max(0.0) as i32,
                )
                .is_ok()
        });
        if !armed {
            // Nothing will clear the flag, and nothing else in the page
            // schedules an attempt: leaving it set would make this the last
            // reconnect the page ever tries.
            self.inner.borrow_mut().reconnect_pending = false;
            log::error!("could not arm the reconnect timer");
        }
    }

    /// One attempt: reopen the socket, redo the handshake on the same pane,
    /// and refetch everything on screen.
    async fn try_reconnect(self: Rc<Self>) {
        let (link, url, token, pane_id, tab_id, size) = {
            let inner = self.inner.borrow();
            if inner.disconnected.is_none() {
                return;
            }
            let size = inner.link.lease().reported;
            (
                inner.link.clone(),
                inner.url.clone(),
                inner.token.clone(),
                inner.pane_id,
                inner.tab_id,
                size,
            )
        };
        let outcome = async {
            link.reconnect(&url, &token).await?;
            crate::attach::reattach(&link, pane_id, tab_id, size).await
        }
        .await;
        match outcome {
            Ok(()) => self.reconnected(),
            Err(err) => {
                // A pane that is gone will not come back, and neither will a
                // server whose protocol this bundle cannot speak. Retrying
                // either one for ever would only hide the reason.
                let permanent = err.downcast_ref::<crate::attach::PaneGone>().is_some()
                    || err.to_string().contains("update the server or the bundle");
                if permanent {
                    // Nothing will read this socket again, and the server
                    // keeps a registered client and a TCP session for as
                    // long as one is open. Hand it back.
                    link.shutdown();
                    let inner = self.inner.borrow();
                    Self::set_status(&inner, &format!("{err:#}"));
                    log::error!("not reconnecting: {err:#}");
                    return;
                }
                let delay = {
                    let mut inner = self.inner.borrow_mut();
                    inner.reconnect_attempts += 1;
                    inner.reconnect_delay =
                        (inner.reconnect_delay * 2.0).clamp(RECONNECT_MIN_MS, RECONNECT_MAX_MS);
                    inner.reconnect_delay
                };
                log::warn!("reconnect failed, retrying in {delay:.0} ms: {err:#}");
                self.show_reconnect_status();
                self.schedule_reconnect(delay);
            }
        }
    }

    /// The socket is back. Everything on screen was fetched from a server
    /// that has since forgotten us, so none of it may be trusted: the rows
    /// go stale and are asked for again.
    fn reconnected(&self) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.disconnected = None;
            inner.reconnect_attempts = 0;
            // The backoff is *not* reset here. A server that accepts and
            // then drops -- one restarting in a loop, a proxy closing idle
            // sockets -- would otherwise be hammered at the minimum delay
            // for ever, because every attempt "succeeds" for the moment it
            // takes to hand back a socket. `frame` clears it once the
            // connection has proved it can carry a frame.
            inner.connected_since = Some(monotonic_ms());
            inner.session.set_dead(false);
            inner.session.make_all_stale();
            log::info!("reconnected to pane {}", inner.pane_id);
        }
        self.refresh_status();
        self.request_frame();
    }

    fn refresh_status(&self) {
        let inner = self.inner.borrow();
        if inner.disconnected.is_some() {
            return;
        }
        let owner = inner.link.lease().owns_viewport();
        let text = if owner {
            format!("{}  ·  {}x{}  ·  this browser has the terminal", inner.title, inner.cols, inner.rows)
        } else {
            format!(
                "{}  ·  {}x{}  ·  following another device (type or click to take over)",
                inner.title, inner.cols, inner.rows
            )
        };
        Self::set_status(&inner, &text);
    }

    fn spawn_drain(inner: &Inner, start: Result<bool, thinkterm_session::input::InputQueueFull>) {
        match start {
            Ok(true) => {
                let session = Arc::clone(&inner.session);
                wasm_bindgen_futures::spawn_local(session.drain_inputs());
                inner.session.update_last_send();
            }
            Ok(false) => inner.session.update_last_send(),
            Err(full) => log::warn!("input refused: {full}"),
        }
    }

    /// Returns true when the key was consumed (the browser must not act
    /// on it). `shift` is the key's real Shift state: for printable keys
    /// it is folded into the character and absent from `mods`, and the
    /// character's case cannot stand in for it (Caps Lock).
    pub fn key_down(&self, key: KeyCode, mods: Modifiers, shift: bool) -> bool {
        let mut inner = self.inner.borrow_mut();
        if inner.composing || inner.disconnected.is_some() {
            return false;
        }
        // Copy and paste chords belong to the page: Cmd+C/V on a Mac,
        // Ctrl+Shift+C/V elsewhere. Copy what is selected; let the paste
        // event carry the clipboard. Every other Cmd chord is the
        // browser's (reload, address bar, tabs), not the pane's.
        let cmd = mods == Modifiers::SUPER;
        let ctrl_shift = mods == Modifiers::CTRL && shift;
        match key {
            KeyCode::Char('c') | KeyCode::Char('C') if cmd || ctrl_shift => {
                if let Some(text) = Self::selection_text(&inner) {
                    write_clipboard(&text);
                }
                return true;
            }
            KeyCode::Char('v') | KeyCode::Char('V') if cmd || ctrl_shift => return false,
            _ if mods.contains(Modifiers::SUPER) => return false,
            _ => {}
        }
        // Typing goes back to following the output.
        inner.scroll_from_bottom = 0;
        inner.selection = None;
        let serial = codec::InputSerial::from_millis(
            thinkterm_session::clock::Clock::wall_millis(&inner.host.clock),
        );
        let mods = KeyModifiers::from_bits_truncate(mods.bits());
        let start = inner.session.key_down(serial, key, mods);
        Self::spawn_drain(&inner, start);
        drop(inner);
        self.request_frame();
        true
    }

    pub fn composing(&self, composing: bool) {
        self.inner.borrow_mut().composing = composing;
    }

    /// Text the IME composed: bytes, not a paste, so no bracketing.
    pub fn text(&self, text: &str) {
        let mut inner = self.inner.borrow_mut();
        if inner.disconnected.is_some() {
            return;
        }
        inner.scroll_from_bottom = 0;
        let start = inner.session.write_bytes(text.as_bytes());
        Self::spawn_drain(&inner, start);
        drop(inner);
        self.request_frame();
    }

    pub fn paste(&self, text: &str) {
        let mut inner = self.inner.borrow_mut();
        if inner.disconnected.is_some() {
            return;
        }
        inner.scroll_from_bottom = 0;
        let start = inner.session.paste(text);
        Self::spawn_drain(&inner, start);
        drop(inner);
        self.request_frame();
    }

    fn cell_under(inner: &Inner, ev: &web_sys::PointerEvent) -> (usize, usize, isize, isize) {
        let rect = inner.canvas.get_bounding_client_rect();
        let px = (ev.client_x() as f64 - rect.left()) * inner.dpr;
        let py = (ev.client_y() as f64 - rect.top()) * inner.dpr;
        let (cw, ch) = (
            inner.glyphs.metrics.cell_size.width as f64,
            inner.glyphs.metrics.cell_size.height as f64,
        );
        let (col, row) = cell_at(px, py, cw, ch);
        let x_off = (px - col as f64 * cw) as isize;
        let y_off = (py - row as f64 * ch) as isize;
        (col, row, x_off, y_off)
    }

    fn mouse_modifiers(ev: &web_sys::MouseEvent) -> KeyModifiers {
        let mut m = KeyModifiers::NONE;
        if ev.shift_key() {
            m |= KeyModifiers::SHIFT;
        }
        if ev.ctrl_key() {
            m |= KeyModifiers::CTRL;
        }
        if ev.alt_key() {
            m |= KeyModifiers::ALT;
        }
        if ev.meta_key() {
            m |= KeyModifiers::SUPER;
        }
        m
    }

    pub fn pointer(&self, ev: &web_sys::PointerEvent, what: Pointer) {
        if what == Pointer::Down {
            // Focus first, outside any borrow: focus() dispatches events
            // synchronously and a listener may look at the app.
            let textarea = self.inner.borrow().textarea.clone();
            let _ = textarea.focus();
            let canvas = self.inner.borrow().canvas.clone();
            let _ = canvas.set_pointer_capture(ev.pointer_id());
        }
        let mut inner = self.inner.borrow_mut();
        if inner.disconnected.is_some() {
            return;
        }
        let (col, row, x_off, y_off) = Self::cell_under(&inner, ev);
        let dims = inner.session.dimensions();
        let visible = visible_rows(&dims, inner.rows, inner.scroll_from_bottom);
        let stable_row = visible.start + row as StableRowIndex;
        let button = match ev.button() {
            0 => MouseButton::Left,
            1 => MouseButton::Middle,
            2 => MouseButton::Right,
            _ => MouseButton::None,
        };

        // A program that asked for the mouse gets it, unless Shift holds
        // the event back for the page's own selection.
        if inner.session.is_mouse_grabbed() && !ev.shift_key() {
            let kind = match what {
                Pointer::Down => MouseEventKind::Press,
                Pointer::Up => MouseEventKind::Release,
                Pointer::Move => MouseEventKind::Move,
            };
            // A follower taller than the pane shows rows above its top;
            // those are nowhere for a program, as on the desktop.
            let event = MouseEvent {
                kind,
                x: col,
                y: (stable_row - dims.physical_top).max(0) as i64,
                x_pixel_offset: x_off,
                y_pixel_offset: y_off,
                button: if what == Pointer::Move && ev.buttons() == 0 { MouseButton::None } else { button },
                modifiers: Self::mouse_modifiers(ev),
            };
            let start = inner.session.mouse_event(event);
            Self::spawn_drain(&inner, start);
            return;
        }

        match what {
            Pointer::Down if button == MouseButton::Left => {
                let t = now_ms();
                inner.click_count = if t - inner.last_click_ms < 400.0 {
                    (inner.click_count % 3) + 1
                } else {
                    1
                };
                inner.last_click_ms = t;
                let mode = inner.click_count;
                inner.selection = Some(Selection {
                    anchor: (stable_row, col),
                    head: (stable_row, col),
                    mode,
                });
                inner.selecting = true;
                // A click is a real interaction: it takes the terminal over.
                let link = inner.link.clone();
                let tab_id = inner.tab_id;
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = link.ensure_owner(tab_id).await;
                });
            }
            Pointer::Move if inner.selecting => {
                if let Some(sel) = &mut inner.selection {
                    sel.head = (stable_row, col);
                }
            }
            Pointer::Up if inner.selecting => {
                inner.selecting = false;
                if let Some(sel) = &mut inner.selection {
                    sel.head = (stable_row, col);
                }
                let empty = inner
                    .selection
                    .map(|s| s.mode == 1 && s.anchor == s.head)
                    .unwrap_or(true);
                if empty {
                    inner.selection = None;
                } else if let Some(text) = Self::selection_text(&inner) {
                    write_clipboard(&text);
                }
            }
            _ => return,
        }
        drop(inner);
        self.request_frame();
    }

    /// The selected cells of `row`, from the selection's shape and mode.
    fn selection_on_row(inner: &Inner, sel: &Selection, row: StableRowIndex, line: &termwiz::surface::Line) -> std::ops::Range<usize> {
        let ((r0, c0), (r1, c1)) = sel.ordered();
        if row < r0 || row > r1 {
            return 0..0;
        }
        let cols = inner.cols.max(line.len());
        match sel.mode {
            3 => 0..cols,
            2 => {
                let word = |col: usize| -> std::ops::Range<usize> {
                    match line.compute_double_click_range(col, |s| {
                        s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
                    }) {
                        termwiz::surface::line::DoubleClickRange::Range(r) => r,
                        termwiz::surface::line::DoubleClickRange::RangeWithWrap(r) => r,
                    }
                };
                let start = if row == r0 { word(c0).start } else { 0 };
                let end = if row == r1 { word(c1).end } else { cols };
                start..end
            }
            _ => {
                let start = if row == r0 { c0 } else { 0 };
                let end = if row == r1 { c1 + 1 } else { cols };
                start..end.max(start)
            }
        }
    }

    fn selection_text(inner: &Inner) -> Option<String> {
        let sel = inner.selection?;
        let ((r0, _), (r1, _)) = sel.ordered();
        let (first, lines) = inner.session.get_lines(r0..r1 + 1);
        let mut out = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let row = first + i as StableRowIndex;
            let range = Self::selection_on_row(inner, &sel, row, line);
            let mut text = String::new();
            for cell in line.visible_cells() {
                if range.contains(&cell.cell_index()) {
                    text.push_str(cell.str());
                }
            }
            out.push(text.trim_end().to_string());
        }
        Some(out.join("\n"))
    }

    /// Returns true when the page acted on the wheel (the browser must
    /// not). A program that has the mouse gets every wheel with its real
    /// modifiers; otherwise Ctrl+wheel and pinch are the browser's zoom.
    pub fn wheel(&self, ev: &web_sys::WheelEvent) -> bool {
        let mut inner = self.inner.borrow_mut();
        if inner.disconnected.is_some() {
            return false;
        }
        let cell_h_css = inner.glyphs.metrics.cell_size.height as f64 / inner.dpr;
        let lines = match ev.delta_mode() {
            web_sys::WheelEvent::DOM_DELTA_LINE => ev.delta_y(),
            web_sys::WheelEvent::DOM_DELTA_PAGE => ev.delta_y() * inner.rows as f64,
            _ => ev.delta_y() / cell_h_css,
        };
        let notches = lines.abs().round().max(if lines == 0.0 { 0.0 } else { 1.0 }) as usize;
        if notches == 0 {
            return false;
        }
        if inner.session.is_alt_screen() || inner.session.is_mouse_grabbed() {
            let button = if lines < 0.0 { MouseButton::WheelUp(notches) } else { MouseButton::WheelDown(notches) };
            let (col, row, x_off, y_off) = {
                let rect = inner.canvas.get_bounding_client_rect();
                let px = (ev.client_x() as f64 - rect.left()) * inner.dpr;
                let py = (ev.client_y() as f64 - rect.top()) * inner.dpr;
                let (cw, ch) = (
                    inner.glyphs.metrics.cell_size.width as f64,
                    inner.glyphs.metrics.cell_size.height as f64,
                );
                let (c, r) = cell_at(px, py, cw, ch);
                (c, r, (px - c as f64 * cw) as isize, (py - r as f64 * ch) as isize)
            };
            let dims = inner.session.dimensions();
            let visible = visible_rows(&dims, inner.rows, inner.scroll_from_bottom);
            let event = MouseEvent {
                kind: MouseEventKind::Press,
                x: col,
                y: (visible.start + row as StableRowIndex - dims.physical_top).max(0) as i64,
                x_pixel_offset: x_off,
                y_pixel_offset: y_off,
                button,
                modifiers: Self::mouse_modifiers(ev),
            };
            let start = inner.session.mouse_event(event);
            Self::spawn_drain(&inner, start);
            return true;
        }
        if ev.ctrl_key() {
            return false;
        }
        let dims = inner.session.dimensions();
        let max = max_scroll(&dims);
        inner.scroll_from_bottom = if lines < 0.0 {
            (inner.scroll_from_bottom + notches).min(max)
        } else {
            inner.scroll_from_bottom.saturating_sub(notches)
        };
        drop(inner);
        self.request_frame();
        true
    }

    pub fn focus(&self, focused: bool) {
        self.inner.borrow_mut().focused = focused;
        self.request_frame();
    }

    pub fn strip_element(&self) -> Option<web_sys::EventTarget> {
        let inner = self.inner.borrow();
        let strip = inner.strip.as_ref()?;
        let target: &web_sys::EventTarget = strip.element().as_ref();
        Some(target.clone())
    }

    /// Draw the tab strip from what the page knows.
    fn render_strip(inner: &Inner) {
        let (Some(strip), Some(layout)) = (&inner.strip, &inner.layout) else {
            return;
        };
        let tabs = crate::chrome::model(layout, inner.pane_id, &inner.title);
        strip.render(&tabs, inner.following);
    }

    /// List the server's panes again and redraw the strip. If the pane on
    /// show is gone, move to the first tab's pane: a dead pane is not
    /// something to look at.
    pub fn refresh_layout(self: &Rc<Self>) {
        {
            let mut inner = self.inner.borrow_mut();
            if inner.layout_refresh_pending || inner.disconnected.is_some() {
                return;
            }
            inner.layout_refresh_pending = true;
        }
        let app = Rc::clone(self);
        wasm_bindgen_futures::spawn_local(async move {
            let link = app.inner.borrow().link.clone();
            let listed = thinkterm_session::host::request(
                &link,
                Pdu::ListPanes(codec::ListPanes {}),
                |pdu| match pdu {
                    Pdu::ListPanesResponse(p) => Ok(p),
                    other => Err(other),
                },
            )
            .await;
            let replacement = {
                let mut inner = app.inner.borrow_mut();
                inner.layout_refresh_pending = false;
                match listed {
                    Ok(layout) => {
                        let gone = crate::chrome::entry(&layout, inner.pane_id).is_none();
                        let replacement = if gone {
                            crate::chrome::first_choice(&layout)
                        } else {
                            None
                        };
                        inner.layout = Some(layout);
                        Self::render_strip(&inner);
                        replacement
                    }
                    Err(err) => {
                        log::warn!("listing panes: {err:#}");
                        None
                    }
                }
            };
            if let Some(entry) = replacement {
                app.switch_to(entry).await;
            }
        });
    }

    /// `refresh_layout`, a moment from now, so a burst of pushes is one
    /// listing.
    fn refresh_layout_soon(self: &Rc<Self>) {
        let app = Rc::clone(self);
        let closure = Closure::once_into_js(move || app.refresh_layout());
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                closure.as_ref().unchecked_ref(),
                150,
            );
        }
    }

    /// Ask the server again every so often, whatever the pushes said:
    /// pane titles elsewhere change without one.
    pub fn poll_layout(self: &Rc<Self>, every_ms: i32) {
        let weak = Rc::downgrade(self);
        let closure = Closure::<dyn FnMut()>::new(move || {
            if let Some(app) = weak.upgrade() {
                app.refresh_layout();
            }
        });
        if let Some(window) = web_sys::window() {
            if window
                .set_interval_with_callback_and_timeout_and_arguments_0(
                    closure.as_ref().unchecked_ref(),
                    every_ms,
                )
                .is_ok()
            {
                // One per page, for the life of the page.
                closure.forget();
            }
        }
    }

    /// A click on the strip.
    pub fn on_chrome_click(self: &Rc<Self>, click: crate::chrome::Click) {
        match click {
            crate::chrome::Click::Pane(pane_id) => self.switch_to_pane(pane_id, true),
            crate::chrome::Click::Follow => {
                let mut inner = self.inner.borrow_mut();
                inner.following = !inner.following;
                Self::render_strip(&inner);
                let _ = inner.textarea.focus();
            }
        }
    }

    /// Show `pane_id`. `chosen` is a person's click, which also stops the
    /// page following the desktop; a focus push is not.
    pub fn switch_to_pane(self: &Rc<Self>, pane_id: PaneId, chosen: bool) {
        if chosen {
            self.inner.borrow_mut().following = false;
        }
        let known = self
            .inner
            .borrow()
            .layout
            .as_ref()
            .and_then(|layout| crate::chrome::entry(layout, pane_id));
        let app = Rc::clone(self);
        wasm_bindgen_futures::spawn_local(async move {
            // Listed again either way: the entry carries the pane's size
            // and screen state, and the listing may be seconds old.
            let link = app.inner.borrow().link.clone();
            let fresh = thinkterm_session::host::request(
                &link,
                Pdu::ListPanes(codec::ListPanes {}),
                |pdu| match pdu {
                    Pdu::ListPanesResponse(p) => Ok(p),
                    other => Err(other),
                },
            )
            .await
            .ok();
            let entry = match fresh {
                Some(layout) => {
                    let entry = crate::chrome::entry(&layout, pane_id);
                    let mut inner = app.inner.borrow_mut();
                    inner.layout = Some(layout);
                    Self::render_strip(&inner);
                    entry
                }
                None => known,
            };
            match entry {
                Some(entry) => app.switch_to(entry).await,
                None => {
                    log::warn!("pane {pane_id} is not on the server");
                    Self::render_strip(&app.inner.borrow());
                }
            }
        });
    }

    /// Put another pane on the canvas: report this page's grid against
    /// its tab, subscribe to it, and start a session for it in place of
    /// the old one. Everything that hangs off the session -- input, the
    /// watchdog, the delta queue -- follows, because it all goes through
    /// `inner.session`.
    async fn switch_to(self: &Rc<Self>, entry: thinkterm_proto::layout::PaneEntry) {
        let (link, size) = {
            let mut inner = self.inner.borrow_mut();
            if entry.pane_id == inner.pane_id || inner.switching || inner.disconnected.is_some() {
                return;
            }
            inner.switching = true;
            let size = inner.link.lease().reported;
            (inner.link.clone(), size)
        };
        let outcome: Result<()> = async {
            // The lease is per tab: a follower's report, never a claim.
            // Typing or clicking claims, as at attach.
            if let Some(size) = size {
                let state = thinkterm_session::host::request(
                    &link,
                    Pdu::SetClientViewport(codec::SetClientViewport {
                        tab_id: entry.tab_id,
                        viewport: codec::ClientViewport::CellGrid { size },
                    }),
                    |pdu| match pdu {
                        Pdu::ClientViewportState(s) => Ok(s),
                        other => Err(other),
                    },
                )
                .await?;
                let mut lease = link.lease_mut();
                lease.tab_id = Some(entry.tab_id);
                lease.apply_viewport(&state);
            } else {
                link.lease_mut().tab_id = Some(entry.tab_id);
            }
            thinkterm_session::host::request(
                &link,
                Pdu::GetPaneRenderChanges(codec::GetPaneRenderChanges {
                    pane_id: entry.pane_id,
                }),
                |pdu| match pdu {
                    Pdu::LivenessResponse(_) | Pdu::UnitResponse(_) => Ok(()),
                    other => Err(other),
                },
            )
            .await?;
            Ok(())
        }
        .await;
        {
            let mut inner = self.inner.borrow_mut();
            inner.switching = false;
            if let Err(err) = outcome {
                log::warn!("switching to pane {}: {err:#}", entry.pane_id);
                Self::set_status(&inner, &format!("could not switch panes: {err:#}"));
                return;
            }
            let rows = entry.size.rows;
            let dims = thinkterm_proto::RenderableDimensions {
                cols: entry.size.cols,
                viewport_rows: rows,
                scrollback_rows: rows,
                physical_top: entry.physical_top,
                scrollback_top: entry.physical_top,
                dpi: entry.size.dpi,
                pixel_width: entry.size.pixel_width,
                pixel_height: entry.size.pixel_height,
                reverse_video: false,
            };
            inner.session = build_session(
                &inner.host,
                entry.pane_id,
                entry.tab_id,
                dims,
                &entry.title,
                entry.alt_screen,
            );
            inner.pane_id = entry.pane_id;
            inner.tab_id = entry.tab_id;
            inner.title = entry.title.clone();
            inner.scroll_from_bottom = 0;
            inner.selection = None;
            inner.selecting = false;
            inner.ime_anchor = None;
            // The palette is a pane's; the new one says its own, if it has
            // one, right after the subscription.
            inner.palette = ColorPalette::default();
            // Forces `resize` to see a change: the new pane is reflowed to
            // this grid if the page owns its tab, or reported against it.
            inner.cols = 0;
            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                doc.set_title(&format!("{} — ThinkTerm", inner.title));
            }
            Self::render_strip(&inner);
            let _ = inner.textarea.focus();
        }
        self.resize();
        self.refresh_status();
        self.request_frame();
    }

    /// See `GlyphCache::warm`. Called once, after the first frame.
    pub fn warm_glyph_canvas(&self) {
        self.inner.borrow_mut().glyphs.warm();
    }

    /// Fit the grid to the canvas's CSS box at the device pixel ratio, and
    /// tell the server if this browser owns the viewport.
    pub fn resize(&self) {
        let mut inner = self.inner.borrow_mut();
        let rect = inner.canvas.get_bounding_client_rect();
        let dpr = web_sys::window().map(|w| w.device_pixel_ratio()).unwrap_or(1.0);
        inner.canvas_rect = [rect.left(), rect.top(), rect.width(), rect.height()];
        if (dpr - inner.dpr).abs() > f64::EPSILON {
            // Another monitor: glyphs are rasterised for the new density.
            inner.dpr = dpr;
            let dpi = (96.0 * dpr) as u32;
            let side = inner.glyphs.atlas.size() as u32;
            match GpuTexture::new(&inner.gpu.device, Arc::clone(&inner.gpu.queue), side, side)
                .and_then(|t| {
                    GlyphCache::new(
                        Rc::clone(&inner.fonts),
                        inner.glyphs.size_pt,
                        dpi,
                        Rc::new(t),
                        Rc::clone(&inner.glyphs.families),
                    )
                })
            {
                Ok(glyphs) => inner.glyphs = glyphs,
                Err(err) => log::error!("glyph cache for dpr {dpr}: {err:#}"),
            }
            // A new cell size makes "what fits" a different question, so
            // the atlas backoff starts over rather than carrying a grudge
            // from the old one.
            inner.capacity.reset();
        }
        let dev_w = (rect.width() * dpr).floor().max(1.0) as u32;
        let dev_h = (rect.height() * dpr).floor().max(1.0) as u32;
        if inner.canvas.width() != dev_w || inner.canvas.height() != dev_h {
            inner.canvas.set_width(dev_w);
            inner.canvas.set_height(dev_h);
        }
        inner.gpu.resize(dev_w, dev_h);
        let (cw, ch) = (
            inner.glyphs.metrics.cell_size.width as u32,
            inner.glyphs.metrics.cell_size.height as u32,
        );
        let Some((cols, rows)) = grid_for(dev_w, dev_h, cw, ch) else {
            // A box too small for a cell is a hidden or collapsed page,
            // not a size anyone asked for: keep the last real grid rather
            // than resize every client's shell to it.
            return;
        };
        let changed = cols != inner.cols || rows != inner.rows;
        inner.cols = cols;
        inner.rows = rows;
        if changed {
            let size = TerminalSize {
                rows,
                cols,
                pixel_width: cols * cw as usize,
                pixel_height: rows * ch as usize,
                dpi: (96.0 * dpr) as u32,
            };
            let owner = inner.link.lease().owns_viewport();
            inner.link.lease_mut().reported = Some(size);
            let link = inner.link.clone();
            let (tab_id, pane_id) = (inner.tab_id, inner.pane_id);
            if owner {
                inner.session.apply_local_resize(size);
                let session = Arc::clone(&inner.session);
                let host = Arc::clone(&inner.host);
                wasm_bindgen_futures::spawn_local(async move {
                    let pdu = Pdu::Resize(codec::Resize {
                        containing_tab_id: tab_id,
                        pane_id,
                        size,
                    });
                    if let Err(err) = thinkterm_session::host::request(&link, pdu, |p| match p {
                        Pdu::UnitResponse(_) => Ok(()),
                        other => Err(other),
                    })
                    .await
                    {
                        // The reflow above was optimistic. Refused, it would
                        // show a grid the server does not have, for ever:
                        // back to the canonical one, and repaint.
                        log::warn!("resize refused: {err:#}");
                        if let Some(canonical) = link.lease().canonical_size {
                            session.apply_local_resize(canonical);
                            session.make_all_stale();
                            host.events.pane_output(0);
                        }
                    }
                });
            } else {
                wasm_bindgen_futures::spawn_local(async move {
                    let pdu = Pdu::SetClientViewport(codec::SetClientViewport {
                        tab_id,
                        viewport: codec::ClientViewport::CellGrid { size },
                    });
                    if let Ok(state) = thinkterm_session::host::request(&link, pdu, |p| match p {
                        Pdu::ClientViewportState(s) => Ok(s),
                        other => Err(other),
                    })
                    .await
                    {
                        link.lease_mut().apply_viewport(&state);
                    }
                });
            }
        }
        drop(inner);
        self.refresh_status();
        self.request_frame();
    }

    /// Paint. Lines the session does not have yet come back blank and are
    /// fetched; their arrival marks the page dirty again.
    pub fn frame(&self) {
        let mut inner = self.inner.borrow_mut();
        // A connection that has carried frames for a while has earned the
        // short delay back. Done here rather than on connect, because
        // "the socket opened" is not evidence a server is healthy.
        if inner
            .connected_since
            .is_some_and(|at| monotonic_ms() - at >= RECONNECT_STABLE_MS)
        {
            inner.connected_since = None;
            inner.reconnect_delay = RECONNECT_MIN_MS;
        }
        let _ = inner.host.events.take_dirty();
        let title = inner.session.title();
        if title != inner.title {
            inner.title = title;
            Self::render_strip(&inner);
            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                doc.set_title(&format!("{} — ThinkTerm", inner.title));
            }
            drop(inner);
            self.refresh_status();
            inner = self.inner.borrow_mut();
        }
        // One budget for the whole frame, not one per attempt. The loop
        // below calls `paint` more than once, so a fresh budget each time
        // would let a single browser callback draw several times the cap --
        // which is the thing the cap is for. The deadline inside it is set
        // once, here, for the same reason.
        let mut budget = FallbackBudget::new(monotonic_ms());
        let (mut owed, mut declined, mut painted) = (0u32, 0u32, false);
        // Whether the atlas is what has been failing, and whether the budget
        // has already been given back once this frame.
        let (mut atlas_failed, mut refunded) = (false, false);
        const ATTEMPTS: usize = 4;
        for attempt in 0..ATTEMPTS {
            // The last attempt always declines allocations, so it cannot
            // fail on the atlas and the frame reaches the screen. Leaving
            // that to `grow_atlas` saying `AtCapacity` was not enough: two
            // growths that both succeed and still do not fit left the last
            // attempt allocating normally, and its failure fell out of the
            // loop with nothing painted, no repaint requested and no timer
            // armed -- the last frame stayed up until unrelated input
            // arrived, which looks exactly like a hang.
            if attempt + 1 == ATTEMPTS && atlas_failed && !inner.capacity.frozen() {
                inner.capacity.at_capacity(monotonic_ms());
            }
            match Self::paint(&mut inner, &mut budget) {
                Ok((deferred, refused)) => {
                    owed = deferred;
                    declined = refused;
                    painted = true;
                    break;
                }
                Err(err) if attempt + 1 < ATTEMPTS => {
                    let Some(full) = err.root_cause().downcast_ref::<OutOfTextureSpace>() else {
                        log::error!("frame failed: {err:#}");
                        break;
                    };
                    atlas_failed = true;
                    match Self::grow_atlas(&mut inner, full.size) {
                        // The cache was rebuilt, so every glyph this frame
                        // has already drawn is gone with it. The retry gets
                        // the count back: charging it twice for the same
                        // graphemes left the rows at the bottom of the
                        // screen -- where a terminal's action is -- as
                        // boxes, and made `owed` a sum over attempts rather
                        // than what this frame still owes.
                        Ok(Grown::Larger) => {
                            // Once only, and the deadline is never given
                            // back -- see `refund`. The rebuild honestly
                            // threw away what the earlier attempt drew, but
                            // three refunds would let one frame do four
                            // times the work, which is the opposite of what
                            // the budget is for.
                            if !refunded {
                                refunded = true;
                                budget.refund();
                            }
                        }
                        Ok(Grown::AtCapacity) => inner.capacity.at_capacity(monotonic_ms()),
                        Err(err) => {
                            log::error!("atlas could not grow: {err:#}");
                            break;
                        }
                    }
                }
                Err(err) => {
                    log::error!("frame failed: {err:#}");
                    break;
                }
            }
        }

        // Lines still in flight, a stalled fetch, or glyphs the budget put
        // off: come back for them.
        let dims = inner.session.dimensions();
        // Exactly the rows shown: asking about rows past a shorter page's
        // bottom, which may not exist, would repaint forever.
        let visible = visible_rows(&dims, inner.rows, inner.scroll_from_bottom);
        // Called first, and never behind a `||`. This is not a predicate:
        // it re-issues line fetches that timed out and resets the poll
        // interval. Short-circuiting it on a frame that deferred a glyph
        // meant a dropped `GetLines` was never retried, and those rows
        // stayed blank for the rest of the stream.
        let stalled = inner.session.render_looks_stalled_in(visible);
        let mut again = owed > 0 || stalled;

        let mut retry_in = None;
        if !painted {
            // Nothing reached the screen. Come back on the backoff's own
            // timer rather than waiting to be asked.
            retry_in = Some(MIN_RETRY_MS);
        } else if inner.capacity.frozen() {
            match inner.capacity.submitted_frozen(monotonic_ms(), owed, declined) {
                Next::Recovered => {}
                Next::Clear => match Self::clear_atlas(&mut inner) {
                    Ok(()) => again = true,
                    Err(err) => {
                        // `submitted_frozen` unfroze on the promise of a
                        // clear that did not happen. Put it back, or the
                        // frame stands still against a full atlas with
                        // nothing scheduled to try again.
                        log::error!("the atlas could not be cleared: {err:#}");
                        inner.capacity.at_capacity(monotonic_ms());
                        retry_in = Some(MIN_RETRY_MS);
                    }
                },
                Next::RetryAt(at) => retry_in = Some((at - monotonic_ms()).max(0.0)),
            }
        } else if owed == 0 && declined == 0 {
            // Owing nothing is the whole condition. A frame whose budget
            // ran out returns successfully too, and taking that for "the
            // demand is satisfied" would end the emergency after refilling
            // only part of the screen.
            inner.capacity.drew_everything();
        }

        drop(inner);
        if again {
            self.request_frame();
        }
        if let Some(delay) = retry_in {
            self.schedule_retry(delay);
        }
    }

    /// Grow the atlas past what would not fit.
    ///
    /// `wanted` is the size the allocator says would hold the sprite, which
    /// is not always twice the current side: one sprite larger than that
    /// does not fit after a single doubling, and the frame only retries so
    /// many times.
    fn grow_atlas(inner: &mut Inner, wanted: Option<usize>) -> Result<Grown> {
        let current = inner.glyphs.atlas.size();
        let side = wanted
            .unwrap_or(current * 2)
            .max(current * 2)
            .next_power_of_two()
            .min(MAX_ATLAS_SIDE)
            .min(inner.gpu.max_texture_dimension() as usize);
        if side <= current {
            return Ok(Grown::AtCapacity);
        }
        let texture = Rc::new(GpuTexture::new(
            &inner.gpu.device,
            Arc::clone(&inner.gpu.queue),
            side as u32,
            side as u32,
        )?);
        Self::rebuild_glyphs(inner, texture)?;
        Ok(Grown::Larger)
    }

    /// Empty the atlas by rebuilding the cache over the same texture:
    /// `Atlas::new` zeroes it and resets the allocator.
    ///
    /// Everything drawn so far is lost, which is the point. There is no
    /// per-sprite eviction, so this is the only way to get space back, and
    /// the flicker it causes is a great deal better than the alternative --
    /// which, before this existed, was a terminal that never drew again.
    fn clear_atlas(inner: &mut Inner) -> Result<()> {
        log::warn!(
            "the atlas is at the GPU's largest texture ({}) and still full; clearing it",
            inner.glyphs.atlas.size()
        );
        let texture = inner.glyphs.texture_rc();
        Self::rebuild_glyphs(inner, texture)
    }

    fn rebuild_glyphs(inner: &mut Inner, texture: Rc<GpuTexture>) -> Result<()> {
        inner.glyphs = GlyphCache::new(
            Rc::clone(&inner.fonts),
            inner.glyphs.size_pt,
            inner.glyphs.dpi,
            texture,
            Rc::clone(&inner.glyphs.families),
        )?;
        Ok(())
    }

    /// Returns what this frame owes: glyphs the budget put off, and sprites
    /// a full atlas declined.
    fn paint(inner: &mut Inner, budget: &mut FallbackBudget) -> Result<(u32, u32)> {
        inner.glyphs.begin_frame(inner.capacity.frozen());
        let dims = inner.session.dimensions();
        let max = max_scroll(&dims);
        if inner.scroll_from_bottom > max {
            inner.scroll_from_bottom = max;
        }
        let cursor = inner.session.cursor_position();
        let visible = visible_rows(&dims, inner.rows, inner.scroll_from_bottom);
        let (first, lines) = inner.session.get_lines(visible);
        let (w, h) = inner.gpu.size();
        let cell_h = inner.glyphs.metrics.cell_size.height as f32;
        let cursor_line = cursor.y.checked_sub(first)
            .and_then(|row| usize::try_from(row).ok())
            .and_then(|row| lines.get(row));
        let width_scale = if cursor_line.is_some_and(|line| !line.is_single_width()) { 2.0 } else { 1.0 };
        let height_scale = if cursor_line.is_some_and(|line| line.is_double_height_top()) { 2.0 } else { 1.0 };
        let cell_w = inner.glyphs.metrics.cell_size.width as f64 * width_scale;
        let anchor = crate::ime::anchor(
            inner.canvas_rect,
            (w, h),
            (cursor.x as f64 * cell_w, cursor.y.saturating_sub(first) as f64 * cell_h as f64),
            (cell_w, cell_h as f64 * height_scale),
        );
        crate::ime::update_field(&inner.textarea, &mut inner.ime_anchor, anchor)
            .map_err(|e| anyhow::anyhow!("IME anchor: {e:?}"))?;
        inner.quads.recycle();
        let selection = inner.selection;
        for (i, line) in lines.iter().enumerate() {
            let row = first + i as StableRowIndex;
            let sel_range = match &selection {
                Some(sel) => Self::selection_on_row(inner, sel, row, line),
                None => 0..0,
            };
            let params = crate::emit::LineParams {
                line,
                stable_row: row,
                top_pixel_y: i as f32 * cell_h,
                cursor: &cursor,
                palette: &inner.palette,
                selection: sel_range,
                focused: inner.focused && inner.link.lease().owns_viewport(),
                reverse_video: dims.reverse_video,
                surface: (w as f32, h as f32),
                origin: (0.0, 0.0),
                clip: (w as f32, h as f32),
                hsv: None,
                draw_cursor: true,
            };
            crate::emit::emit_line(&mut inner.glyphs, &mut inner.quads, budget, &params)?;
        }
        inner.vertices.clear();
        inner.quads.extract_vertices(&mut inner.vertices);
        let bg = inner.palette.background.to_linear().tuple();
        let millis = (js_sys::Date::now() % (u32::MAX as f64)) as u32;
        inner
            .gpu
            .draw(&inner.vertices, inner.glyphs.texture(), [bg.0, bg.1, bg.2, bg.3], millis)?;
        Ok((budget.deferred(), inner.glyphs.declined()))
    }
}

/// The grid a canvas of `w`x`h` device pixels holds, or `None` when it
/// cannot hold even one cell: a 2x1 shell is never what a 0x0 box means.
pub fn grid_for(w: u32, h: u32, cell_w: u32, cell_h: u32) -> Option<(usize, usize)> {
    if cell_w == 0 || cell_h == 0 || w < cell_w || h < cell_h {
        return None;
    }
    Some(((w / cell_w).max(2) as usize, (h / cell_h).max(1) as usize))
}

#[cfg(test)]
mod tests {
    use super::grid_for;

    #[test]
    fn a_degenerate_box_yields_no_grid() {
        assert_eq!(grid_for(0, 0, 10, 20), None);
        assert_eq!(grid_for(1, 1, 10, 20), None);
        assert_eq!(grid_for(800, 19, 10, 20), None);
        assert_eq!(grid_for(800, 400, 10, 20), Some((80, 20)));
        assert_eq!(grid_for(15, 20, 10, 20), Some((2, 1)));
    }
}

fn write_clipboard(text: &str) {
    if let Some(window) = web_sys::window() {
        let clipboard = window.navigator().clipboard();
        let promise = clipboard.write_text(text);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(err) = wasm_bindgen_futures::JsFuture::from(promise).await {
                log::warn!("clipboard write refused: {err:?}");
            }
        });
    }
}
