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
pub struct Selection {
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
/// How long the close button waits for its second press.
const CLOSE_CONFIRM_MS: f64 = 3_000.0;
/// The desktop's `inactive_pane_hsb` default: panes without the focus are
/// a little darker and a little greyer.
const INACTIVE_PANE_HSB: wezterm_color_types::HsbTransform = wezterm_color_types::HsbTransform {
    hue: 1.0,
    saturation: 0.9,
    brightness: 0.8,
};

pub struct Setup {
    pub link: WsLink,
    pub host: Arc<WebHost>,
    pub images: Arc<thinkterm_session::Lock<thinkterm_session::images::ImageStore>>,
    pub remote_tab_id: Arc<std::sync::atomic::AtomicUsize>,
    /// The pane on show, already built with `build_session`.
    pub pane: PaneCell,
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
    pub window_id: thinkterm_proto::WindowId,
    pub workspace: String,
    pub dpr: f64,
    pub cols: usize,
    pub rows: usize,
    pub strip: Option<crate::chrome::TabStrip>,
}

/// One pane on the page: its session and the state that is the page's
/// own for it. Everything here is per pane; what is per page lives on
/// `Inner`.
/// A point on the canvas, resolved to a pane and a cell within it.
#[derive(Debug, Clone, Copy)]
struct Hit {
    pane_id: PaneId,
    col: usize,
    row: usize,
    x_off: isize,
    y_off: isize,
}

pub struct PaneCell {
    pub session: Arc<PaneSession<WebHost>>,
    pub scroll_from_bottom: usize,
    pub selection: Option<Selection>,
    /// The pane's own colours (OSC 4/10/11), pushed as `SetApplicationPalette`.
    pub palette: ColorPalette,
    pub title: String,
}

impl PaneCell {
    pub fn new(session: Arc<PaneSession<WebHost>>, title: &str) -> Self {
        Self {
            session,
            scroll_from_bottom: 0,
            selection: None,
            palette: ColorPalette::default(),
            title: title.to_string(),
        }
    }
}

pub struct Inner {
    link: WsLink,
    /// The panes of the tab on show, by remote pane id. Never empty: the
    /// focused pane's cell stays (dead, if need be) until a listing puts
    /// something else in its place.
    panes: std::collections::BTreeMap<PaneId, PaneCell>,
    focused_pane: PaneId,
    host: Arc<WebHost>,
    /// One per connection, shared by every session: a picture one pane
    /// fetched is not fetched again by another.
    images: Arc<thinkterm_session::Lock<thinkterm_session::images::ImageStore>>,
    /// One per shown tab, shared by its sessions, so a move updates all.
    remote_tab_id: Arc<std::sync::atomic::AtomicUsize>,
    gpu: Gpu,
    glyphs: GlyphCache,
    fonts: Rc<FontSet>,
    canvas: web_sys::HtmlCanvasElement,
    textarea: web_sys::HtmlTextAreaElement,
    status: Option<web_sys::Element>,
    tab_id: TabId,
    window_id: thinkterm_proto::WindowId,
    workspace: String,
    dpr: f64,
    cols: usize,
    rows: usize,
    /// Word/line selection extends by units; the pointer is down.
    selecting: bool,
    /// The pane a drag started in; it keeps the drag until the release.
    drag_pane: Option<PaneId>,
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
    /// What to do about an atlas that has run out of room for good.
    capacity: Capacity,
    /// The server's tabs and panes as last listed, for the strip and for
    /// switching. Refreshed on the pushes that change it and on a timer.
    layout: Option<codec::ListPanesResponse>,
    /// Where the panes of the tab on show go, from that listing.
    tab_layout: Option<crate::layout::TabLayout>,
    /// Whether the page moves to whatever pane the desktop focuses. On by
    /// default: a page opened to "see my terminal" wants the one being
    /// used. Choosing a pane here turns it off.
    following: bool,
    strip: Option<crate::chrome::TabStrip>,
    layout_refresh_pending: bool,
    /// A switch is under way; a second request waits for the next push
    /// or click rather than racing it.
    switching: bool,
    /// The close button was pressed once; the second press, within a
    /// few seconds, is the one that closes.
    closing_since: Option<f64>,
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
    images: &Arc<thinkterm_session::Lock<thinkterm_session::images::ImageStore>>,
    remote_tab_id: &Arc<std::sync::atomic::AtomicUsize>,
    pane_id: PaneId,
    dims: thinkterm_proto::RenderableDimensions,
    title: &str,
    alt_screen: bool,
) -> Arc<PaneSession<WebHost>> {
    PaneSession::new(
        Arc::clone(host),
        Arc::clone(images),
        thinkterm_session::SessionConfig {
            scrollback_lines: 3500,
            local_echo_threshold_ms: Some(100),
            overlay_lag_indicator: false,
        },
        pane_id,
        Arc::clone(remote_tab_id),
        // The host's name for the pane is the server's: it is what the
        // session's events carry, and the page keys its panes by it.
        pane_id,
        dims,
        title,
        alt_screen,
    )
}

impl Inner {
    fn focused(&self) -> &PaneCell {
        self.panes
            .get(&self.focused_pane)
            .expect("the focused pane has a cell")
    }

    fn focused_mut(&mut self) -> &mut PaneCell {
        let id = self.focused_pane;
        self.panes.get_mut(&id).expect("the focused pane has a cell")
    }

    fn title(&self) -> &str {
        &self.focused().title
    }
}

impl App {
    pub fn new(setup: Setup) -> Rc<Self> {
        let mut panes = std::collections::BTreeMap::new();
        panes.insert(setup.pane_id, setup.pane);
        let inner = Inner {
            link: setup.link,
            panes,
            focused_pane: setup.pane_id,
            host: setup.host,
            images: setup.images,
            remote_tab_id: setup.remote_tab_id,
            gpu: setup.gpu,
            glyphs: setup.glyphs,
            fonts: setup.fonts,
            canvas: setup.canvas,
            textarea: setup.textarea,
            status: setup.status,
            tab_id: setup.tab_id,
            window_id: setup.window_id,
            workspace: setup.workspace,
            dpr: setup.dpr,
            cols: setup.cols,
            rows: setup.rows,
            selecting: false,
            drag_pane: None,
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
            layout: None,
            tab_layout: None,
            following: true,
            strip: setup.strip,
            layout_refresh_pending: false,
            switching: false,
            closing_since: None,
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
            // Every pane on the server pushes to every client; only the
            // ones on this page are wanted, and the rest are simply not
            // ours (never a reason to re-list: there are many).
            Pdu::GetPaneRenderChangesResponse(delta) => {
                if let Some(cell) = inner.panes.get(&delta.pane_id) {
                    cell.session.queue_render_delta(delta);
                }
            }
            Pdu::PaneRemoved(removed) if inner.panes.contains_key(&removed.pane_id) => {
                let cell = &inner.panes[&removed.pane_id];
                cell.session.set_dead(true);
                if removed.pane_id == inner.focused_pane {
                    Self::set_status(&inner, "the pane was closed on the server");
                }
                // Something else to show, if the server has anything.
                drop(inner);
                self.refresh_layout();
            }
            // The desktop moved. Followed only while following; a page
            // that chose a pane is not dragged off it.
            Pdu::PaneFocused(focused) if inner.following && focused.pane_id != inner.focused_pane => {
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
                if inner.panes.contains_key(&pane_id) =>
            {
                drop(inner);
                let mut inner = self.inner.borrow_mut();
                let palette = palette.unwrap_or_default();
                let cell = inner.panes.get_mut(&pane_id).expect("checked above");
                if palette != cell.palette {
                    cell.palette = palette;
                    cell.session.make_all_stale();
                    drop(inner);
                    self.request_frame();
                }
            }
            Pdu::ClientViewportState(_) | Pdu::FrontendAccessState(_) => {
                let (link, tab_id) = (inner.link.clone(), inner.tab_id);
                drop(inner);
                // The desktop may have resized: keep the server's copy of
                // this page's report at the tab's size, or the next
                // keystroke would claim the old one.
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(err) = link.report_canonical(tab_id).await {
                        log::warn!("reporting the tab size: {err:#}");
                    }
                });
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
            for cell in inner.panes.values() {
                cell.session.set_dead(true);
            }
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
                inner.focused_pane,
                inner.tab_id,
                size,
            )
        };
        let outcome = async {
            link.reconnect(&url, &token).await?;
            crate::attach::reattach(&link, tab_id, pane_id, size).await
        }
        .await;
        match outcome {
            Ok(list) => {
                self.reconnected();
                // The tab is laid out again from the fresh listing: panes
                // that came or went while the socket was down are taken
                // up or let go, and the ones that stayed keep their cells.
                self.show(list, pane_id).await;
            }
            Err(err) => {
                // A server with nothing to show will not grow something by
                // being asked again, and neither will one whose protocol
                // this bundle cannot speak. Retrying either one for ever
                // would only hide the reason.
                let permanent = err.downcast_ref::<crate::attach::NoPanes>().is_some()
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
            for cell in inner.panes.values() {
                cell.session.set_dead(false);
                cell.session.make_all_stale();
            }
            log::info!("reconnected to pane {}", inner.focused_pane);
        }
        self.refresh_status();
        self.request_frame();
    }

    fn focused_placement(inner: &Inner) -> Option<crate::layout::PanePlacement> {
        Self::placements(inner)
            .into_iter()
            .find(|p| p.pane_id == inner.focused_pane)
    }

    fn refresh_status(&self) {
        let inner = self.inner.borrow();
        if inner.disconnected.is_some() {
            return;
        }
        let owner = inner.link.lease().owns_viewport();
        let (cols, rows) = inner
            .tab_layout
            .as_ref()
            .map(|l| (l.cols, l.rows))
            .unwrap_or((inner.cols, inner.rows));
        let lease = inner.link.lease();
        let mut text = if owner {
            format!("{}  ·  {}x{}  ·  this browser has the terminal", inner.title(), cols, rows)
        } else if !lease.may_type() {
            format!(
                "{}  ·  {}x{}  ·  another device has the terminal (Ctrl+Shift+T to take over)",
                inner.title(), cols, rows
            )
        } else {
            format!("{}  ·  {}x{}  ·  mirroring the desktop", inner.title(), cols, rows)
        };
        if lease.fit {
            text.push_str("  ·  fitted to this window (Ctrl+Shift+F again to let go)");
        }
        drop(lease);
        if cols > inner.cols || rows > inner.rows {
            text.push_str(&format!("  ·  this window fits {}x{}", inner.cols, inner.rows));
        }
        Self::set_status(&inner, &text);
    }

    fn spawn_drain(
        session: &Arc<PaneSession<WebHost>>,
        start: Result<bool, thinkterm_session::input::InputQueueFull>,
    ) {
        match start {
            Ok(true) => {
                wasm_bindgen_futures::spawn_local(Arc::clone(session).drain_inputs());
                session.update_last_send();
            }
            Ok(false) => session.update_last_send(),
            Err(full) => log::warn!("input refused: {full}"),
        }
    }

    /// Returns true when the key was consumed (the browser must not act
    /// on it). `shift` is the key's real Shift state: for printable keys
    /// it is folded into the character and absent from `mods`, and the
    /// character's case cannot stand in for it (Caps Lock).
    pub fn key_down(self: &Rc<Self>, key: KeyCode, mods: Modifiers, shift: bool) -> bool {
        let mut inner = self.inner.borrow_mut();
        if inner.composing || inner.disconnected.is_some() {
            return false;
        }
        if !Self::may_type(&inner) {
            return true;
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
            // Ctrl+Shift+Enter splits to the right, Ctrl+Shift+\ below,
            // Ctrl+Shift+Z zooms; the same as the strip's buttons.
            KeyCode::Enter if ctrl_shift => {
                drop(inner);
                self.split(thinkterm_proto::SplitDirection::Horizontal);
                return true;
            }
            KeyCode::Char('\\') | KeyCode::Char('|') if ctrl_shift => {
                drop(inner);
                self.split(thinkterm_proto::SplitDirection::Vertical);
                return true;
            }
            KeyCode::Char('z') | KeyCode::Char('Z') if ctrl_shift => {
                drop(inner);
                self.toggle_zoom();
                return true;
            }
            // Ctrl+Shift+F fits the tab to this window; Ctrl+Shift+T
            // takes the terminal over from another device (Handoff mode).
            KeyCode::Char('f') | KeyCode::Char('F') if ctrl_shift => {
                drop(inner);
                self.fit(true);
                return true;
            }
            KeyCode::Char('t') | KeyCode::Char('T') if ctrl_shift => {
                drop(inner);
                self.take_over();
                return true;
            }
            // Ctrl+Shift+arrow moves the focus between the tab's panes,
            // locally: dragging the desktop along is a click's job.
            KeyCode::LeftArrow | KeyCode::RightArrow | KeyCode::UpArrow | KeyCode::DownArrow
                if mods == Modifiers::CTRL | Modifiers::SHIFT || ctrl_shift =>
            {
                let next = Self::neighbour(&inner, key);
                drop(inner);
                if let Some(next) = next {
                    self.focus_pane(next, false);
                }
                return true;
            }
            _ if mods.contains(Modifiers::SUPER) => return false,
            _ => {}
        }
        let serial = codec::InputSerial::from_millis(
            thinkterm_session::clock::Clock::wall_millis(&inner.host.clock),
        );
        let mods = KeyModifiers::from_bits_truncate(mods.bits());
        // Typing goes back to following the output.
        let cell = inner.focused_mut();
        cell.scroll_from_bottom = 0;
        cell.selection = None;
        let start = cell.session.key_down(serial, key, mods);
        Self::spawn_drain(&cell.session, start);
        drop(inner);
        self.request_frame();
        true
    }

    pub fn composing(&self, composing: bool) {
        self.inner.borrow_mut().composing = composing;
    }

    /// Whether input may go out at all. In Handoff mode with the
    /// terminal in someone else's hands the server would drop it; the
    /// status line says so and offers to take over.
    fn may_type(inner: &Inner) -> bool {
        if inner.link.lease().may_type() {
            return true;
        }
        Self::set_status(
            inner,
            "another device is using this terminal; press Ctrl+Shift+T to take it over",
        );
        false
    }

    /// Text the IME composed: bytes, not a paste, so no bracketing.
    pub fn text(&self, text: &str) {
        let mut inner = self.inner.borrow_mut();
        if inner.disconnected.is_some() || !Self::may_type(&inner) {
            return;
        }
        let cell = inner.focused_mut();
        cell.scroll_from_bottom = 0;
        let start = cell.session.write_bytes(text.as_bytes());
        Self::spawn_drain(&cell.session, start);
        drop(inner);
        self.request_frame();
    }

    pub fn paste(&self, text: &str) {
        let mut inner = self.inner.borrow_mut();
        if inner.disconnected.is_some() || !Self::may_type(&inner) {
            return;
        }
        let cell = inner.focused_mut();
        cell.scroll_from_bottom = 0;
        let start = cell.session.paste(text);
        Self::spawn_drain(&cell.session, start);
        drop(inner);
        self.request_frame();
    }

    /// Where a point on the canvas lands: which pane, and the cell within
    /// it. `None` on a divider or past the tab.
    fn hit_under(inner: &Inner, client_x: f64, client_y: f64) -> Option<Hit> {
        let rect = inner.canvas.get_bounding_client_rect();
        let px = (client_x - rect.left()) * inner.dpr;
        let py = (client_y - rect.top()) * inner.dpr;
        let (cw, ch) = (
            inner.glyphs.metrics.cell_size.width as f64,
            inner.glyphs.metrics.cell_size.height as f64,
        );
        let (col, row) = cell_at(px, py, cw, ch);
        let place = match &inner.tab_layout {
            Some(layout) => crate::layout::hit(layout, col, row)?.clone(),
            None => Self::focused_placement(inner)?,
        };
        Some(Self::hit_in(&place, col, row, px, py, cw, ch))
    }

    /// The cell of `place` under a canvas cell, clamped into the pane:
    /// a drag that leaves the pane keeps reporting its edge.
    fn hit_in(place: &crate::layout::PanePlacement, col: usize, row: usize, px: f64, py: f64, cw: f64, ch: f64) -> Hit {
        let (cols, rows) = Self::shown(place);
        let local_col = col.saturating_sub(place.frame.left).min(cols.saturating_sub(1));
        let local_row = row.saturating_sub(place.frame.top).min(rows.saturating_sub(1));
        let x_off = (px - (place.frame.left + local_col) as f64 * cw).clamp(0.0, cw) as isize;
        let y_off = (py - (place.frame.top + local_row) as f64 * ch).clamp(0.0, ch) as isize;
        Hit {
            pane_id: place.pane_id,
            col: local_col,
            row: local_row,
            x_off,
            y_off,
        }
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

    /// Focus a pane that is on the page. `advise` tells the server, so
    /// the desktop follows: a click does when the page is following, a
    /// focus push (which came from the server) never does.
    pub fn focus_pane(self: &Rc<Self>, pane_id: PaneId, advise: bool) {
        let link = {
            let mut inner = self.inner.borrow_mut();
            if !inner.panes.contains_key(&pane_id) || inner.focused_pane == pane_id {
                return;
            }
            inner.focused_pane = pane_id;
            inner.selecting = false;
            inner.drag_pane = None;
            inner.ime_anchor = None;
            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                doc.set_title(&format!("{} — ThinkTerm", inner.title()));
            }
            Self::render_strip(&inner);
            inner.link.clone()
        };
        self.refresh_status();
        self.request_frame();
        if advise {
            // Sent and forgotten. The desktop may ignore it for a few
            // seconds after a focus move of its own, and the server's echo
            // is the pane already focused here.
            wasm_bindgen_futures::spawn_local(async move {
                let pdu = Pdu::SetFocusedPane(codec::SetFocusedPane {
                    pane_id,
                    configured_palette: None,
                });
                if let Err(err) = thinkterm_session::host::request(&link, pdu, |p| match p {
                    Pdu::UnitResponse(_) => Ok(()),
                    other => Err(other),
                })
                .await
                {
                    log::warn!("focus not advised: {err:#}");
                }
            });
        }
    }

    /// The pane next to the focused one in `direction`: the nearest whose
    /// frame lies past the focused frame's edge on that side.
    fn neighbour(inner: &Inner, direction: KeyCode) -> Option<PaneId> {
        let layout = inner.tab_layout.as_ref()?;
        let me = layout.panes.iter().find(|p| p.pane_id == inner.focused_pane)?;
        let (mx, my) = (
            me.frame.left as isize + me.frame.cols as isize / 2,
            me.frame.top as isize + me.frame.rows as isize / 2,
        );
        layout
            .panes
            .iter()
            .filter(|p| p.pane_id != me.pane_id)
            .filter(|p| match direction {
                KeyCode::LeftArrow => p.frame.left + p.frame.cols <= me.frame.left,
                KeyCode::RightArrow => p.frame.left >= me.frame.left + me.frame.cols,
                KeyCode::UpArrow => p.frame.top + p.frame.rows <= me.frame.top,
                KeyCode::DownArrow => p.frame.top >= me.frame.top + me.frame.rows,
                _ => false,
            })
            .min_by_key(|p| {
                let (x, y) = (
                    p.frame.left as isize + p.frame.cols as isize / 2,
                    p.frame.top as isize + p.frame.rows as isize / 2,
                );
                (x - mx).abs() + (y - my).abs()
            })
            .map(|p| p.pane_id)
    }

    pub fn pointer(self: &Rc<Self>, ev: &web_sys::PointerEvent, what: Pointer) {
        if what == Pointer::Down {
            // Focus first, outside any borrow: focus() dispatches events
            // synchronously and a listener may look at the app.
            let textarea = self.inner.borrow().textarea.clone();
            let _ = textarea.focus();
            let canvas = self.inner.borrow().canvas.clone();
            let _ = canvas.set_pointer_capture(ev.pointer_id());
        }
        // A press lands on the pane under it and focuses it; a drag stays
        // with the pane it started in.
        let hit = {
            let inner = self.inner.borrow();
            if inner.disconnected.is_some() {
                return;
            }
            match (what, inner.drag_pane) {
                (Pointer::Down, _) => Self::hit_under(&inner, ev.client_x() as f64, ev.client_y() as f64),
                (_, Some(drag)) => {
                    let place = Self::placements(&inner).into_iter().find(|p| p.pane_id == drag);
                    place.map(|place| {
                        let rect = inner.canvas.get_bounding_client_rect();
                        let px = (ev.client_x() as f64 - rect.left()) * inner.dpr;
                        let py = (ev.client_y() as f64 - rect.top()) * inner.dpr;
                        let (cw, ch) = (
                            inner.glyphs.metrics.cell_size.width as f64,
                            inner.glyphs.metrics.cell_size.height as f64,
                        );
                        let (col, row) = cell_at(px.max(0.0), py.max(0.0), cw, ch);
                        Self::hit_in(&place, col, row, px, py, cw, ch)
                    })
                }
                (_, None) => Self::hit_under(&inner, ev.client_x() as f64, ev.client_y() as f64),
            }
        };
        let Some(hit) = hit else {
            return;
        };
        if what == Pointer::Down {
            let advise = self.inner.borrow().following;
            self.focus_pane(hit.pane_id, advise);
        }
        let mut inner = self.inner.borrow_mut();
        if what == Pointer::Down && !Self::may_type(&inner) {
            return;
        }
        let Some(cell) = inner.panes.get(&hit.pane_id) else {
            return;
        };
        let session = Arc::clone(&cell.session);
        let scroll = cell.scroll_from_bottom;
        let dims = session.dimensions();
        let rows_shown = Self::placements(&inner)
            .into_iter()
            .find(|p| p.pane_id == hit.pane_id)
            .map(|p| Self::shown(&p).1)
            .unwrap_or(inner.rows);
        let visible = visible_rows(&dims, rows_shown, scroll);
        let stable_row = visible.start + hit.row as StableRowIndex;
        let button = match ev.button() {
            0 => MouseButton::Left,
            1 => MouseButton::Middle,
            2 => MouseButton::Right,
            _ => MouseButton::None,
        };

        // A program that asked for the mouse gets it, unless Shift holds
        // the event back for the page's own selection.
        if session.is_mouse_grabbed() && !ev.shift_key() {
            let kind = match what {
                Pointer::Down => MouseEventKind::Press,
                Pointer::Up => MouseEventKind::Release,
                Pointer::Move => MouseEventKind::Move,
            };
            // A follower taller than the pane shows rows above its top;
            // those are nowhere for a program, as on the desktop.
            let event = MouseEvent {
                kind,
                x: hit.col,
                y: (stable_row - dims.physical_top).max(0) as i64,
                x_pixel_offset: hit.x_off,
                y_pixel_offset: hit.y_off,
                button: if what == Pointer::Move && ev.buttons() == 0 { MouseButton::None } else { button },
                modifiers: Self::mouse_modifiers(ev),
            };
            let start = session.mouse_event(event);
            Self::spawn_drain(&session, start);
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
                if let Some(cell) = inner.panes.get_mut(&hit.pane_id) {
                    cell.selection = Some(Selection {
                        anchor: (stable_row, hit.col),
                        head: (stable_row, hit.col),
                        mode,
                    });
                }
                inner.selecting = true;
                inner.drag_pane = Some(hit.pane_id);
                // A click is a real interaction: it takes the terminal over.
                let link = inner.link.clone();
                let tab_id = inner.tab_id;
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = link.ensure_owner(tab_id).await;
                });
            }
            Pointer::Move if inner.selecting => {
                if let Some(sel) = inner.panes.get_mut(&hit.pane_id).and_then(|c| c.selection.as_mut()) {
                    sel.head = (stable_row, hit.col);
                }
            }
            Pointer::Up if inner.selecting => {
                inner.selecting = false;
                inner.drag_pane = None;
                if let Some(sel) = inner.panes.get_mut(&hit.pane_id).and_then(|c| c.selection.as_mut()) {
                    sel.head = (stable_row, hit.col);
                }
                let empty = inner
                    .panes
                    .get(&hit.pane_id)
                    .and_then(|c| c.selection)
                    .map(|s| s.mode == 1 && s.anchor == s.head)
                    .unwrap_or(true);
                if empty {
                    if let Some(cell) = inner.panes.get_mut(&hit.pane_id) {
                        cell.selection = None;
                    }
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
    fn selection_on_row(width: usize, sel: &Selection, row: StableRowIndex, line: &termwiz::surface::Line) -> std::ops::Range<usize> {
        let ((r0, c0), (r1, c1)) = sel.ordered();
        if row < r0 || row > r1 {
            return 0..0;
        }
        let cols = width.max(line.len());
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
        let sel = inner.focused().selection?;
        let ((r0, _), (r1, _)) = sel.ordered();
        let (first, lines) = inner.focused().session.get_lines(r0..r1 + 1);
        let width = Self::focused_placement(inner).map(|p| Self::shown(&p).0).unwrap_or(inner.cols);
        let mut out = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let row = first + i as StableRowIndex;
            let range = Self::selection_on_row(width, &sel, row, line);
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
    /// not). The pane under the pointer scrolls, as on the desktop. A
    /// program that has the mouse gets every wheel with its real
    /// modifiers; otherwise Ctrl+wheel and pinch are the browser's zoom.
    pub fn wheel(&self, ev: &web_sys::WheelEvent) -> bool {
        let mut inner = self.inner.borrow_mut();
        if inner.disconnected.is_some() {
            return false;
        }
        let Some(hit) = Self::hit_under(&inner, ev.client_x() as f64, ev.client_y() as f64) else {
            return false;
        };
        let rows_shown = Self::placements(&inner)
            .into_iter()
            .find(|p| p.pane_id == hit.pane_id)
            .map(|p| Self::shown(&p).1)
            .unwrap_or(inner.rows);
        let cell_h_css = inner.glyphs.metrics.cell_size.height as f64 / inner.dpr;
        let lines = match ev.delta_mode() {
            web_sys::WheelEvent::DOM_DELTA_LINE => ev.delta_y(),
            web_sys::WheelEvent::DOM_DELTA_PAGE => ev.delta_y() * rows_shown as f64,
            _ => ev.delta_y() / cell_h_css,
        };
        let notches = lines.abs().round().max(if lines == 0.0 { 0.0 } else { 1.0 }) as usize;
        if notches == 0 {
            return false;
        }
        let Some(cell) = inner.panes.get_mut(&hit.pane_id) else {
            return false;
        };
        let session = Arc::clone(&cell.session);
        if session.is_alt_screen() || session.is_mouse_grabbed() {
            let button = if lines < 0.0 { MouseButton::WheelUp(notches) } else { MouseButton::WheelDown(notches) };
            let dims = session.dimensions();
            let visible = visible_rows(&dims, rows_shown, cell.scroll_from_bottom);
            let event = MouseEvent {
                kind: MouseEventKind::Press,
                x: hit.col,
                y: (visible.start + hit.row as StableRowIndex - dims.physical_top).max(0) as i64,
                x_pixel_offset: hit.x_off,
                y_pixel_offset: hit.y_off,
                button,
                modifiers: Self::mouse_modifiers(ev),
            };
            let start = session.mouse_event(event);
            Self::spawn_drain(&session, start);
            return true;
        }
        if ev.ctrl_key() {
            return false;
        }
        let max = max_scroll(&session.dimensions());
        cell.scroll_from_bottom = if lines < 0.0 {
            (cell.scroll_from_bottom + notches).min(max)
        } else {
            cell.scroll_from_bottom.saturating_sub(notches)
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

    /// What the page has laid out, as JSON on the canvas element for the
    /// smoke tests and anyone else curious: `canvas.dataset.layout`.
    /// Rewritten whenever the strip is, which is whenever it changes.
    fn publish_layout(inner: &Inner) {
        let lease = inner.link.lease();
        let dpr = inner.dpr.max(0.1);
        let (cw, ch) = (
            inner.glyphs.metrics.cell_size.width as f64 / dpr,
            inner.glyphs.metrics.cell_size.height as f64 / dpr,
        );
        let mut json = format!(
            "{{\"tab\":{},\"canvas\":[{},{}],\"cell\":[{cw:.3},{ch:.3}],\"focused\":{},\"owner\":{},\"may_type\":{},\"fit\":{},\"following\":{}",
            inner.tab_id,
            inner.cols,
            inner.rows,
            inner.focused_pane,
            lease.owns_viewport(),
            lease.may_type(),
            lease.fit,
            inner.following,
        );
        match &inner.tab_layout {
            Some(layout) => {
                json.push_str(&format!(",\"cols\":{},\"rows\":{},\"zoomed\":", layout.cols, layout.rows));
                match layout.zoomed {
                    Some(id) => json.push_str(&id.to_string()),
                    None => json.push_str("null"),
                }
                json.push_str(",\"panes\":[");
                for (i, p) in layout.panes.iter().enumerate() {
                    if i > 0 {
                        json.push(',');
                    }
                    json.push_str(&format!(
                        "{{\"id\":{},\"left\":{},\"top\":{},\"cols\":{},\"rows\":{},\"content\":[{},{}]}}",
                        p.pane_id, p.frame.left, p.frame.top, p.frame.cols, p.frame.rows, p.content.0, p.content.1
                    ));
                }
                json.push_str("],\"dividers\":[");
                for (i, d) in layout.dividers.iter().enumerate() {
                    if i > 0 {
                        json.push(',');
                    }
                    match *d {
                        crate::layout::Divider::Col { col, top, rows } => {
                            json.push_str(&format!("{{\"col\":{col},\"top\":{top},\"rows\":{rows}}}"))
                        }
                        crate::layout::Divider::Row { row, left, cols } => {
                            json.push_str(&format!("{{\"row\":{row},\"left\":{left},\"cols\":{cols}}}"))
                        }
                    }
                }
                json.push(']');
            }
            None => json.push_str(",\"cols\":null,\"rows\":null,\"zoomed\":null,\"panes\":[],\"dividers\":[]"),
        }
        json.push('}');
        let _ = inner.canvas.set_attribute("data-layout", &json);
    }

    /// Draw the tab strip from what the page knows.
    fn render_strip(inner: &Inner) {
        Self::publish_layout(inner);
        let (Some(strip), Some(layout)) = (&inner.strip, &inner.layout) else {
            return;
        };
        let tabs = crate::chrome::model(layout, inner.focused_pane, inner.title());
        let controls = crate::chrome::Controls {
            following: inner.following,
            zoomed: inner.tab_layout.as_ref().is_some_and(|l| l.zoomed.is_some()),
            fit: inner.link.lease().fit,
            closing: inner
                .closing_since
                .is_some_and(|since| monotonic_ms() - since < CLOSE_CONFIRM_MS),
        };
        strip.render(&tabs, controls);
    }

    /// List the server's panes again, redraw the strip, and bring the tab
    /// on show up to date: new panes get sessions, gone ones lose them. If
    /// the focused pane is gone, the first tab's pane is shown instead.
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
            let listed = app.list_panes().await;
            app.inner.borrow_mut().layout_refresh_pending = false;
            let Some(list) = listed else {
                return;
            };
            let want = app.inner.borrow().focused_pane;
            app.show(list, want).await;
        });
    }

    async fn list_panes(&self) -> Option<codec::ListPanesResponse> {
        let link = self.inner.borrow().link.clone();
        match thinkterm_session::host::request(
            &link,
            Pdu::ListPanes(codec::ListPanes {}),
            |pdu| match pdu {
                Pdu::ListPanesResponse(p) => Ok(p),
                other => Err(other),
            },
        )
        .await
        {
            Ok(list) => Some(list),
            Err(err) => {
                log::warn!("listing panes: {err:#}");
                None
            }
        }
    }

    /// Put the tab holding `want` on the canvas with `want` focused, from
    /// a fresh listing. The same tab is reconciled in place; another tab
    /// is reported to the server first, since the lease is per tab.
    async fn show(self: &Rc<Self>, list: codec::ListPanesResponse, want: PaneId) {
        let node = crate::layout::tab_containing(&list, want)
            .or_else(|| list.tabs.iter().find(|t| crate::layout::layout(t).is_some()));
        let Some(layout) = node.and_then(crate::layout::layout) else {
            let mut inner = self.inner.borrow_mut();
            inner.layout = Some(list);
            Self::render_strip(&inner);
            Self::set_status(&inner, "the server has no panes to show");
            return;
        };
        let (link, tab_changed) = {
            let mut inner = self.inner.borrow_mut();
            if inner.switching || inner.disconnected.is_some() {
                return;
            }
            let tab_changed = layout.tab_id != inner.tab_id;
            inner.switching = tab_changed;
            (inner.link.clone(), tab_changed)
        };
        if tab_changed {
            // The lease is per tab: a follower's report of the tab's own
            // size, never a claim. Typing or clicking claims, as at attach.
            let outcome: Result<()> = async {
                {
                    let mut lease = link.lease_mut();
                    lease.tab_id = Some(layout.tab_id);
                    lease.tab_owner = None;
                    lease.canonical_size = Some(layout.size);
                    lease.reported_canonical = None;
                    lease.fit = false;
                }
                link.report_canonical(layout.tab_id).await
            }
            .await;
            let mut inner = self.inner.borrow_mut();
            inner.switching = false;
            if let Err(err) = outcome {
                log::warn!("switching to tab {}: {err:#}", layout.tab_id);
                Self::set_status(&inner, &format!("could not switch tabs: {err:#}"));
                return;
            }
        }
        let (fresh, changed) = {
            let mut inner = self.inner.borrow_mut();
            inner.layout = Some(list);
            let before = (inner.focused_pane, inner.tab_layout.clone());
            let fresh = Self::apply_layout(&mut inner, layout, want);
            let changed = tab_changed
                || !fresh.is_empty()
                || before.0 != inner.focused_pane
                || before.1 != inner.tab_layout;
            if tab_changed {
                inner.selecting = false;
                inner.ime_anchor = None;
                // Forces `resize` to see a change: the tab is reflowed to
                // this grid if the page owns it, or reported against it.
                inner.cols = 0;
            }
            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                doc.set_title(&format!("{} — ThinkTerm", inner.title()));
            }
            Self::render_strip(&inner);
            (fresh, changed)
        };
        // A pane's first push comes when something asks after it: one
        // liveness poll each, and the answer is not waited for.
        for pane_id in fresh {
            let link = link.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let _ = thinkterm_session::host::request(
                    &link,
                    Pdu::GetPaneRenderChanges(codec::GetPaneRenderChanges { pane_id }),
                    |pdu| match pdu {
                        Pdu::LivenessResponse(_) | Pdu::UnitResponse(_) => Ok(()),
                        other => Err(other),
                    },
                )
                .await;
            });
        }
        if tab_changed {
            self.resize();
        }
        // A listing that changed nothing (the timer's, mostly) is not a
        // reason to paint.
        if changed {
            self.refresh_status();
            self.request_frame();
        }
    }

    /// Make `inner.panes` match `layout`, focusing `want` if it is drawn
    /// and the tab's active pane otherwise. Returns the panes that are
    /// new to the page.
    fn apply_layout(inner: &mut Inner, layout: crate::layout::TabLayout, want: PaneId) -> Vec<PaneId> {
        if layout.tab_id != inner.tab_id {
            // Another tab is another lease and another tab id for every
            // session under it.
            inner.remote_tab_id = Arc::new(std::sync::atomic::AtomicUsize::new(layout.tab_id));
            inner.panes.clear();
            inner.tab_id = layout.tab_id;
        }
        inner.window_id = layout.window_id;
        inner.workspace = layout.workspace.clone();
        let drawn: std::collections::BTreeSet<PaneId> =
            layout.panes.iter().map(|p| p.pane_id).collect();
        inner.panes.retain(|id, _| drawn.contains(id));
        let mut fresh = Vec::new();
        for place in &layout.panes {
            if inner.panes.contains_key(&place.pane_id) {
                continue;
            }
            let rows = place.size.rows;
            let dims = thinkterm_proto::RenderableDimensions {
                cols: place.size.cols,
                viewport_rows: rows,
                scrollback_rows: rows,
                physical_top: place.physical_top,
                scrollback_top: place.physical_top,
                dpi: place.size.dpi,
                pixel_width: place.size.pixel_width,
                pixel_height: place.size.pixel_height,
                reverse_video: false,
            };
            let session = build_session(
                &inner.host,
                &inner.images,
                &inner.remote_tab_id,
                place.pane_id,
                dims,
                &place.title,
                place.alt_screen,
            );
            inner
                .panes
                .insert(place.pane_id, PaneCell::new(session, &place.title));
            fresh.push(place.pane_id);
        }
        inner.focused_pane = if drawn.contains(&want) {
            want
        } else {
            layout
                .panes
                .iter()
                .find(|p| p.is_active)
                .or(layout.panes.first())
                .map(|p| p.pane_id)
                .unwrap_or(want)
        };
        inner.tab_layout = Some(layout);
        fresh
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
        use crate::chrome::Click;
        // Any other click withdraws a pending close.
        if click != Click::Close {
            self.inner.borrow_mut().closing_since = None;
        }
        match click {
            Click::Pane(pane_id) => self.switch_to_pane(pane_id, true),
            Click::Follow => {
                let mut inner = self.inner.borrow_mut();
                inner.following = !inner.following;
                Self::render_strip(&inner);
                let _ = inner.textarea.focus();
            }
            Click::NewTab => self.new_tab(),
            Click::SplitRight => self.split(thinkterm_proto::SplitDirection::Horizontal),
            Click::SplitBelow => self.split(thinkterm_proto::SplitDirection::Vertical),
            Click::Zoom => self.toggle_zoom(),
            Click::Fit => {
                let on = !self.inner.borrow().link.lease().fit;
                self.fit(on);
            }
            Click::Close => {
                let confirmed = {
                    let mut inner = self.inner.borrow_mut();
                    let now = monotonic_ms();
                    match inner.closing_since {
                        Some(since) if now - since < CLOSE_CONFIRM_MS => {
                            inner.closing_since = None;
                            true
                        }
                        _ => {
                            inner.closing_since = Some(now);
                            Self::render_strip(&inner);
                            let _ = inner.textarea.focus();
                            false
                        }
                    }
                };
                if confirmed {
                    self.close_pane();
                } else {
                    // Back to a plain × when the moment passes.
                    let app = Rc::clone(self);
                    let closure = Closure::once_into_js(move || {
                        let inner = app.inner.borrow();
                        Self::render_strip(&inner);
                    });
                    if let Some(window) = web_sys::window() {
                        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                            closure.as_ref().unchecked_ref(),
                            CLOSE_CONFIRM_MS as i32 + 50,
                        );
                    }
                }
            }
        }
    }

    /// One request to the server, its refusal shown rather than logged,
    /// and the layout listed again afterwards; the server's own pushes
    /// list it too, which is harmless.
    fn act<F>(self: &Rc<Self>, what: &'static str, pdu: Pdu, done: F)
    where
        F: FnOnce(&Rc<Self>, Pdu) + 'static,
    {
        let link = self.inner.borrow().link.clone();
        let app = Rc::clone(self);
        wasm_bindgen_futures::spawn_local(async move {
            match thinkterm_session::host::request(&link, pdu, Ok).await {
                Ok(answer) => {
                    done(&app, answer);
                    app.refresh_layout();
                }
                Err(err) => {
                    log::warn!("{what}: {err:#}");
                    let inner = app.inner.borrow();
                    Self::set_status(&inner, &format!("could not {what}: {err:#}"));
                }
            }
            let _ = app.inner.borrow().textarea.focus();
        });
    }

    /// A new tab in the window this tab is in, at the tab's own size.
    pub fn new_tab(self: &Rc<Self>) {
        let (window_id, workspace, size) = {
            let inner = self.inner.borrow();
            let size = inner
                .link
                .lease()
                .canonical_size
                .or_else(|| inner.tab_layout.as_ref().map(|l| l.size))
                .unwrap_or_default();
            (inner.window_id, inner.workspace.clone(), size)
        };
        self.act(
            "open a new tab",
            Pdu::SpawnV2(codec::SpawnV2 {
                domain: thinkterm_proto::SpawnTabDomain::CurrentPaneDomain,
                window_id: Some(window_id),
                command: None,
                command_dir: None,
                size,
                workspace,
            }),
            |app, answer| {
                if let Pdu::SpawnResponse(spawned) = answer {
                    // The page moves with it; the desktop already has.
                    app.switch_to_pane(spawned.pane_id, false);
                }
            },
        );
    }

    /// Split the focused pane; the new pane takes the focus.
    pub fn split(self: &Rc<Self>, direction: thinkterm_proto::SplitDirection) {
        let pane_id = self.inner.borrow().focused_pane;
        self.act(
            "split the pane",
            Pdu::SplitPane(codec::SplitPane {
                pane_id,
                split_request: thinkterm_proto::SplitRequest {
                    direction,
                    target_is_second: true,
                    top_level: false,
                    size: thinkterm_proto::SplitSize::Percent(50),
                },
                command: None,
                command_dir: None,
                domain: thinkterm_proto::SpawnTabDomain::CurrentPaneDomain,
                move_pane_id: None,
            }),
            |app, answer| {
                if let Pdu::SpawnResponse(spawned) = answer {
                    app.switch_to_pane(spawned.pane_id, false);
                }
            },
        );
    }

    /// Zoom the focused pane to the whole tab, or back.
    pub fn toggle_zoom(self: &Rc<Self>) {
        let (tab_id, pane_id, zoomed) = {
            let inner = self.inner.borrow();
            let zoomed = inner.tab_layout.as_ref().is_some_and(|l| l.zoomed.is_some());
            (inner.tab_id, inner.focused_pane, zoomed)
        };
        self.act(
            "zoom the pane",
            Pdu::SetPaneZoomed(codec::SetPaneZoomed {
                containing_tab_id: tab_id,
                pane_id,
                zoomed: !zoomed,
            }),
            |_, _| {},
        );
    }

    /// Close the focused pane, ending its program.
    pub fn close_pane(self: &Rc<Self>) {
        let pane_id = self.inner.borrow().focused_pane;
        self.act(
            "close the pane",
            Pdu::KillPane(codec::KillPane { pane_id }),
            |_, _| {},
        );
    }

    /// The pane at `pane_id`, and the page's focus with it. `chosen` is a
    /// person's click, which also stops the page following the desktop;
    /// a focus push is not.
    pub fn switch_to_pane(self: &Rc<Self>, pane_id: PaneId, chosen: bool) {
        if chosen {
            self.inner.borrow_mut().following = false;
        }
        // Already drawn: a focus change and nothing else. Not advised to
        // the server: a strip click has just pinned the page, and a focus
        // push is the server's own news.
        if self.inner.borrow().panes.contains_key(&pane_id) {
            self.focus_pane(pane_id, false);
            let _ = self.inner.borrow().textarea.focus();
            return;
        }
        // Elsewhere: listed again either way, since the entry carries the
        // pane's size and screen state and the listing may be seconds old.
        let app = Rc::clone(self);
        wasm_bindgen_futures::spawn_local(async move {
            let Some(list) = app.list_panes().await else {
                return;
            };
            if crate::layout::tab_containing(&list, pane_id).is_none() {
                log::warn!("pane {pane_id} is not on the server");
                let mut inner = app.inner.borrow_mut();
                inner.layout = Some(list);
                Self::render_strip(&inner);
                return;
            }
            app.show(list, pane_id).await;
        });
    }

    /// Reshape the tab to this window's grid (`on`), or go back to the
    /// desktop's size (`off`): a claim either way, at the size the lease
    /// then picks.
    pub fn fit(self: &Rc<Self>, on: bool) {
        let (link, tab_id) = {
            let inner = self.inner.borrow();
            inner.link.lease_mut().fit = on;
            (inner.link.clone(), inner.tab_id)
        };
        let app = Rc::clone(self);
        wasm_bindgen_futures::spawn_local(async move {
            match link.claim(tab_id).await {
                Ok(_) => {
                    app.refresh_status();
                    app.request_frame();
                }
                Err(err) => {
                    let inner = app.inner.borrow();
                    Self::set_status(&inner, &format!("could not resize the tab: {err:#}"));
                }
            }
        });
    }

    /// Take the terminal from whichever device holds it (Handoff mode).
    /// There is no giving it back: the desktop takes it by interacting.
    pub fn take_over(self: &Rc<Self>) {
        let (link, tab_id) = {
            let inner = self.inner.borrow();
            (inner.link.clone(), inner.tab_id)
        };
        let app = Rc::clone(self);
        wasm_bindgen_futures::spawn_local(async move {
            match link.claim(tab_id).await {
                Ok(true) => {
                    app.refresh_status();
                    app.request_frame();
                }
                Ok(false) => {
                    let inner = app.inner.borrow();
                    Self::set_status(&inner, "the server did not hand the terminal over");
                }
                Err(err) => {
                    let inner = app.inner.borrow();
                    Self::set_status(&inner, &format!("could not take over: {err:#}"));
                }
            }
        });
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
            // The page's grid is its own business: the tab keeps the
            // desktop's size and this canvas letterboxes it. Only a page
            // that asked to fit the tab to itself sends its grid, and then
            // as a claim, because that is what changing the tab's size is.
            let size = TerminalSize {
                rows,
                cols,
                pixel_width: cols * cw as usize,
                pixel_height: rows * ch as usize,
                dpi: (96.0 * dpr) as u32,
            };
            let fitting = {
                let mut lease = inner.link.lease_mut();
                lease.reported = Some(size);
                lease.fit && lease.owns_viewport()
            };
            if fitting {
                let link = inner.link.clone();
                let tab_id = inner.tab_id;
                wasm_bindgen_futures::spawn_local(async move {
                    if let Err(err) = link.claim(tab_id).await {
                        log::warn!("fitting the tab to this window: {err:#}");
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
        let title = inner.focused().session.title();
        if title != inner.focused().title {
            inner.focused_mut().title = title;
            Self::render_strip(&inner);
            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                doc.set_title(&format!("{} — ThinkTerm", inner.title()));
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
        // Exactly the rows shown: asking about rows past a shorter page's
        // bottom, which may not exist, would repaint forever.
        // Called first, and never behind a `||`. This is not a predicate:
        // it re-issues line fetches that timed out and resets the poll
        // interval. Short-circuiting it on a frame that deferred a glyph
        // meant a dropped `GetLines` was never retried, and those rows
        // stayed blank for the rest of the stream. Every pane, every frame.
        let mut stalled = false;
        for place in Self::placements(&inner) {
            let Some(cell) = inner.panes.get(&place.pane_id) else {
                continue;
            };
            let dims = cell.session.dimensions();
            let visible = visible_rows(&dims, Self::shown(&place).1, cell.scroll_from_bottom);
            stalled |= cell.session.render_looks_stalled_in(visible);
        }
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

    /// The panes to draw and where. Before the first listing there is
    /// one, at the canvas's origin and the page's own grid.
    fn placements(inner: &Inner) -> Vec<crate::layout::PanePlacement> {
        if let Some(layout) = &inner.tab_layout {
            return layout.panes.clone();
        }
        let cell = inner.focused();
        let size = cell.session.dimensions();
        vec![crate::layout::PanePlacement {
            pane_id: inner.focused_pane,
            tab_id: inner.tab_id,
            window_id: inner.window_id,
            frame: crate::layout::Rect { left: 0, top: 0, cols: inner.cols, rows: inner.rows },
            content: (inner.cols, inner.rows),
            is_active: true,
            is_zoomed: false,
            title: cell.title.clone(),
            alt_screen: false,
            physical_top: size.physical_top,
            size: TerminalSize::default(),
            workspace: inner.workspace.clone(),
        }]
    }

    /// The grid a placement shows: its frame, or less when the pane's own
    /// grid is smaller (the desktop reserves rows of a frame for chrome).
    fn shown(place: &crate::layout::PanePlacement) -> (usize, usize) {
        (place.frame.cols.min(place.content.0), place.frame.rows.min(place.content.1))
    }

    /// Returns what this frame owes: glyphs the budget put off, and sprites
    /// a full atlas declined.
    fn paint(inner: &mut Inner, budget: &mut FallbackBudget) -> Result<(u32, u32)> {
        inner.glyphs.begin_frame(inner.capacity.frozen());
        let (w, h) = inner.gpu.size();
        let surface = (w as f32, h as f32);
        let cell_w = inner.glyphs.metrics.cell_size.width as f32;
        let cell_h = inner.glyphs.metrics.cell_size.height as f32;
        let owns = inner.link.lease().owns_viewport();
        let placements = Self::placements(inner);
        let focused_palette = inner.focused().palette.clone();
        inner.quads.recycle();
        for place in &placements {
            let Some(cell) = inner.panes.get_mut(&place.pane_id) else {
                continue;
            };
            let session = Arc::clone(&cell.session);
            let dims = session.dimensions();
            let max = max_scroll(&dims);
            if cell.scroll_from_bottom > max {
                cell.scroll_from_bottom = max;
            }
            let (cols_shown, rows_shown) = Self::shown(place);
            let visible = visible_rows(&dims, rows_shown, cell.scroll_from_bottom);
            let (first, lines) = session.get_lines(visible);
            let cursor = session.cursor_position();
            let palette = cell.palette.clone();
            let selection = cell.selection;
            let is_focused = place.pane_id == inner.focused_pane;
            let hsv = if is_focused { None } else { Some(INACTIVE_PANE_HSB) };
            let origin = (place.frame.left as f32 * cell_w, place.frame.top as f32 * cell_h);
            let clip = (cols_shown as f32 * cell_w, rows_shown as f32 * cell_h);
            // The pane's own ground, over its whole frame: it carries the
            // pane's colours, and covers the chrome rows of a frame the
            // desktop reserved.
            crate::emit::fill_rect(
                &inner.glyphs,
                &mut inner.quads,
                0,
                surface,
                origin,
                (place.frame.cols as f32 * cell_w, place.frame.rows as f32 * cell_h),
                palette.background.to_linear(),
                hsv,
            )?;
            if is_focused {
                let cursor_line = cursor
                    .y
                    .checked_sub(first)
                    .and_then(|row| usize::try_from(row).ok())
                    .and_then(|row| lines.get(row));
                let width_scale = if cursor_line.is_some_and(|line| !line.is_single_width()) { 2.0 } else { 1.0 };
                let height_scale = if cursor_line.is_some_and(|line| line.is_double_height_top()) { 2.0 } else { 1.0 };
                let cw = cell_w as f64 * width_scale;
                let anchor = crate::ime::anchor(
                    inner.canvas_rect,
                    (w, h),
                    (
                        origin.0 as f64 + cursor.x as f64 * cw,
                        origin.1 as f64 + cursor.y.saturating_sub(first) as f64 * cell_h as f64,
                    ),
                    (cw, cell_h as f64 * height_scale),
                );
                crate::ime::update_field(&inner.textarea, &mut inner.ime_anchor, anchor)
                    .map_err(|e| anyhow::anyhow!("IME anchor: {e:?}"))?;
            }
            for (i, line) in lines.iter().enumerate() {
                let row = first + i as StableRowIndex;
                let sel_range = match &selection {
                    Some(sel) => Self::selection_on_row(cols_shown, sel, row, line),
                    None => 0..0,
                };
                let params = crate::emit::LineParams {
                    line,
                    stable_row: row,
                    top_pixel_y: i as f32 * cell_h,
                    cursor: &cursor,
                    palette: &palette,
                    selection: sel_range,
                    focused: inner.focused && owns,
                    reverse_video: dims.reverse_video,
                    surface,
                    origin,
                    clip,
                    hsv,
                    draw_cursor: is_focused,
                };
                crate::emit::emit_line(&mut inner.glyphs, &mut inner.quads, budget, &params)?;
            }
        }
        // Dividers, centred in the gap cell like the desktop's.
        if let Some(layout) = &inner.tab_layout {
            let t = (inner.glyphs.metrics.underline_height.max(1)) as f32;
            let colour = focused_palette.split.to_linear();
            for divider in &layout.dividers {
                let (x, y, dw, dh) = match *divider {
                    crate::layout::Divider::Col { col, top, rows } => (
                        col as f32 * cell_w + (cell_w - t) / 2.0,
                        top as f32 * cell_h,
                        t,
                        rows as f32 * cell_h,
                    ),
                    crate::layout::Divider::Row { row, left, cols } => (
                        left as f32 * cell_w,
                        row as f32 * cell_h + (cell_h - t) / 2.0,
                        cols as f32 * cell_w,
                        t,
                    ),
                };
                crate::emit::fill_rect(&inner.glyphs, &mut inner.quads, 0, surface, (x, y), (dw, dh), colour, None)?;
            }
        }
        inner.vertices.clear();
        inner.quads.extract_vertices(&mut inner.vertices);
        let bg = focused_palette.background.to_linear().tuple();
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
