//! The page's state: one pane session, the canvas it is drawn on, where
//! the user is looking, what they selected, and who owns the tab.

use crate::fallback::{Capacity, FallbackBudget, Next, MIN_RETRY_MS};
use crate::glyphs::GlyphCache;
use crate::gpu::Gpu;
use crate::host::WebHost;
use crate::link::WsLink;
use crate::viewport::{cell_at, max_scroll, visible_rows, visible_rows_px};
use anyhow::Result;
use codec::Pdu;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use termwiz::input::{KeyCode, Modifiers};
use thinkterm_font_core::FontShaper as _;
use thinkterm_i18n::tr;
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
    /// `?font=` was given: the size is the user's, not the desktop's.
    pub font_pinned: bool,
    /// The browser's languages, for switching the locale by preference.
    pub languages: Vec<String>,
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
    /// How far the top visible row is cut off at its top, in device px,
    /// always inside `[0, cell_h)`: the part of a row that smooth
    /// scrolling is through. Zero means the view sits on a row boundary,
    /// which is the only position stepped mode ever holds and the only
    /// one either end of the scrollback allows.
    pub scroll_px: f32,
    pub selection: Option<Selection>,
    /// The pane's own colours (OSC 4/10/11), pushed as `SetApplicationPalette`,
    /// exactly as they arrived. `None` means the base palette shows through.
    pub application: Option<ColorPalette>,
    /// What is drawn: the base palette under `application`, if any.
    pub palette: ColorPalette,
    pub title: String,
    /// A live-resize preview in force: its epoch, size, and when it
    /// began, so one the server never confirms is not kept for good.
    pub preview: Option<(u64, TerminalSize, f64)>,
    /// Parked through a reconnect: its rows may have moved on without
    /// this page hearing, so it is polled afresh when next shown.
    pub reconnect_stale: bool,
}

impl PaneCell {
    pub fn new(session: Arc<PaneSession<WebHost>>, title: &str) -> Self {
        Self {
            preview: None,
            reconnect_stale: false,
            session,
            scroll_from_bottom: 0,
            scroll_px: 0.0,
            selection: None,
            application: None,
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
    /// Cells of tabs the page has shown and left: kept, and kept current
    /// by the server's pushes, so switching back shows them at once with
    /// their lines, cursor and colours rather than a blank round trip.
    parked: std::collections::BTreeMap<PaneId, PaneCell>,
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
    tab_id: TabId,
    /// Tabs this page has shown, most recent first: a thread opens on
    /// the tab it was left at, not the window's active one, since the
    /// page's tab switches are its own and never move the server's.
    recent_tabs: Vec<TabId>,
    window_id: thinkterm_proto::WindowId,
    workspace: String,
    dpr: f64,
    cols: usize,
    rows: usize,
    font_pinned: bool,
    /// The page's own size: what it shows while it holds the tab, and
    /// what the settings pin. Following the desktop's cell is for a
    /// follower only.
    base_size_pt: f64,
    /// The size the page started with: what "follow" goes back to.
    boot_size_pt: f64,
    languages: Vec<String>,
    /// Palette picks, most recent first; the page keeps them across loads.
    recent: Vec<String>,
    /// The page's preferences, as it handed them over.
    settings: crate::settings::WebSettings,
    /// The palette the server's own configuration resolves to, pushed as
    /// `DefaultPalette` right after the handshake.
    server_palette: Option<ColorPalette>,
    /// A scheme this browser picked, which outranks the server's.
    chosen_palette: Option<ColorPalette>,
    /// The background last published on the canvas as `data-bg`.
    published_bg: Option<String>,
    /// Word/line selection extends by units; the pointer is down.
    selecting: bool,
    /// The pane a drag started in; it keeps the drag until the release.
    drag_pane: Option<PaneId>,
    /// A divider being dragged: which, where the press was along its
    /// axis (device px), and the cells already sent to the server.
    drag_divider: Option<(usize, f64, isize)>,
    /// The layout as the divider drag began: every step is measured
    /// from it, not from the last step's result.
    drag_layout: Option<crate::layout::TabLayout>,
    /// A divider claim on the wire, and the step that arrived meanwhile
    /// to send when it answers: one round trip at a time, the newest
    /// position wins.
    divider_claim_busy: bool,
    divider_claim_next: Option<(usize, isize)>,
    /// Counts live-resize previews, so a late answer cannot end a newer one.
    preview_epoch: u64,
    /// A sidebar or panel edge is being dragged: the canvas follows it,
    /// the tab is reshaped once when it is let go.
    panel_drag: bool,
    /// Every pane's application palette the server has pushed, on the page
    /// or not: it is sent once per connection, so a pane drawn again after
    /// a tab switch takes its colours from here.
    overrides: std::collections::HashMap<PaneId, Option<ColorPalette>>,
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
    /// Where a row that hangs over the edge of its pane's content box is
    /// recorded before being replayed cropped. One row at a time, emptied
    /// and refilled: at most two rows per pane per frame go through it,
    /// and it is never allocated inside the paint.
    scratch: HeapQuadAllocator,
    /// Panes at their own font size (Cmd+= / Cmd+-), as on the desktop:
    /// each has its own glyph cache and atlas, and is drawn as its own
    /// batch. Absent means the page's size.
    pane_fonts: std::collections::BTreeMap<PaneId, PaneFont>,
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
    /// The tab the page last claimed on its own, on showing it; see
    /// `show`. A tab is claimed once per showing, not on every listing.
    auto_claimed: Option<TabId>,
    /// The page is in its phone shape (`body[data-mobile]`), read at each
    /// resize: the grid is padded differently there.
    mobile: bool,
    /// The page's display layer, told once per animation frame that
    /// something it shows changed; it reads the views it wants.
    on_change: Option<js_sys::Function>,
    notify_pending: Rc<Cell<bool>>,
    /// What the status views report: the current remark and the summary.
    toast: RefCell<Option<crate::views::Toast>>,
    summary: RefCell<String>,
    /// The timer that hides the passing remark, so a new one restarts it.
    tree: crate::tree::TreeModel,
    session_refresh_pending: bool,
    /// A thread's delete button was pressed once, and when.
    deleting: Option<(String, f64)>,
    editing: crate::sidebar::Editing,
    /// Why the last path typed into "Add workspace" was refused.
    new_project_error: Option<String>,
    /// A tab's close button was pressed once, and when.
    closing_tab: Option<(TabId, f64)>,
    layout_refresh_pending: bool,
    /// A switch is under way; a second request waits for the next push
    /// or click rather than racing it.
    switching: bool,
    /// The close button was pressed once; the second press, within a
    /// few seconds, is the one that closes.
    closing_since: Option<f64>,
    /// What the interaction that asked for the terminal wanted done once
    /// the server hands it over.
    after_take_over: Option<AfterTakeOver>,
    /// How the last ask for the terminal went, for the card to say.
    claim: Claim,
}

/// One pane's own font: its scale over the page's size, and what draws it.
struct PaneFont {
    scale: f64,
    glyphs: GlyphCache,
    quads: HeapQuadAllocator,
    vertices: Vec<Vertex>,
}

/// Continuation of a take-over that an interaction started.
/// How many tabs' cells are kept parked when the page leaves them.
const PARKED_TABS: usize = 12;

/// The page's last ask for the terminal (Handoff mode): none, on the
/// wire, or turned down by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Claim {
    Idle,
    Taking,
    Refused,
}

enum AfterTakeOver {
    Focus(PaneId),
    Key(KeyCode, Modifiers, bool),
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
    /// The base palette every pane draws from: this browser's pick, else
    /// the server's configured scheme, else the stock palette.
    pub fn configured(&self) -> ColorPalette {
        crate::settings::configured_palette(
            self.chosen_palette.as_ref(),
            self.server_palette.as_ref(),
        )
    }

    /// Fold the base palette and each pane's application override back into
    /// what is drawn. Returns the panes whose palette moved.
    fn recompute_palettes(&mut self) -> Vec<PaneId> {
        let configured = self.configured();
        let mut changed = Vec::new();
        for (pane_id, cell) in self.panes.iter_mut() {
            if let Some(palette) = crate::settings::recomputed_palette(
                &cell.palette,
                &configured,
                cell.application.as_ref(),
            ) {
                cell.palette = palette;
                changed.push(*pane_id);
            }
        }
        changed
    }

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

/// The toast for a request that failed: `what` is a `web-act-*` key.
fn failed(what: &str, err: &dyn std::fmt::Display) -> String {
    let mut args = thinkterm_i18n::FluentArgs::new();
    args.set("what", tr(what));
    args.set("error", format!("{err:#}"));
    thinkterm_i18n::tr_args("web-toast-failed", &args)
}

impl App {
    pub fn new(setup: Setup) -> Rc<Self> {
        let mut panes = std::collections::BTreeMap::new();
        panes.insert(setup.pane_id, setup.pane);
        let inner = Inner {
            link: setup.link,
            panes,
            parked: std::collections::BTreeMap::new(),
            focused_pane: setup.pane_id,
            host: setup.host,
            images: setup.images,
            remote_tab_id: setup.remote_tab_id,
            gpu: setup.gpu,
            base_size_pt: setup.glyphs.size_pt,
            boot_size_pt: setup.glyphs.size_pt,
            glyphs: setup.glyphs,
            fonts: setup.fonts,
            canvas: setup.canvas,
            textarea: setup.textarea,
            tab_id: setup.tab_id,
            recent_tabs: vec![setup.tab_id],
            window_id: setup.window_id,
            workspace: setup.workspace,
            dpr: setup.dpr,
            cols: setup.cols,
            rows: setup.rows,
            selecting: false,
            drag_pane: None,
            drag_divider: None,
            drag_layout: None,
            divider_claim_busy: false,
            divider_claim_next: None,
            preview_epoch: 0,
            panel_drag: false,
            overrides: std::collections::HashMap::new(),
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
            font_pinned: setup.font_pinned,
            languages: setup.languages,
            recent: Vec::new(),
            settings: crate::settings::WebSettings::default(),
            server_palette: None,
            chosen_palette: None,
            published_bg: None,
            quads: HeapQuadAllocator::default(),
            vertices: Vec::new(),
            scratch: HeapQuadAllocator::default(),
            pane_fonts: Default::default(),
            layout: None,
            tab_layout: None,
            following: true,
            auto_claimed: None,
            mobile: false,
            on_change: None,
            notify_pending: Rc::new(Cell::new(false)),
            toast: RefCell::new(None),
            summary: RefCell::new(String::new()),
            tree: crate::tree::TreeModel::default(),
            session_refresh_pending: false,
            deleting: None,
            editing: crate::sidebar::Editing::None,
            new_project_error: None,
            closing_tab: None,
            layout_refresh_pending: false,
            switching: false,
            closing_since: None,
            after_take_over: None,
            claim: Claim::Idle,
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

    /// A passing remark, bottom right, gone after a few seconds -- the
    /// desktop has no status line, and neither does the page. It stays
    /// while the connection is down: that is not a remark but the state.
    fn set_status(inner: &Inner, text: &str) {
        *inner.toast.borrow_mut() = Some(crate::views::Toast {
            text: text.to_string(),
            sticky: inner.disconnected.is_some(),
            at: monotonic_ms(),
        });
        if inner.disconnected.is_some() {
            // Not a remark but the state: probes read it there too.
            *inner.summary.borrow_mut() = text.to_string();
        }
        Self::notify(inner);
    }

    pub fn hide_status(&self) {
        let inner = self.inner.borrow();
        *inner.toast.borrow_mut() = None;
        Self::notify(&inner);
    }

    /// The desktop's surface over a terminal another device holds, or
    /// none when this page may type. It says who holds it, and after a
    /// press, whether the server let go.
    fn card(inner: &Inner) -> Option<crate::views::Card> {
        use crate::views::Card;
        let lease = inner.link.lease();
        if !matches!(lease.mode, Some(codec::FrontendAccessMode::Handoff)) || lease.owns_viewport() {
            return None;
        }
        let host = lease.owner.as_ref().map(|o| o.hostname.trim().to_string()).unwrap_or_default();
        let taking = tr("web-card-take-over");
        Some(match (inner.claim, lease.owner.is_some()) {
            (Claim::Taking, _) => Card {
                title: tr("web-card-taking-title"),
                hint: tr("web-card-taking-hint"),
                state: "taking",
                action: taking,
            },
            (Claim::Refused, true) => Card {
                title: tr("web-card-refused-title"),
                hint: tr("web-card-refused-hint"),
                state: "refused",
                action: tr("web-card-try-again"),
            },
            (_, true) => Card {
                title: if host.is_empty() {
                    tr("web-card-busy-title")
                } else {
                    let mut args = thinkterm_i18n::FluentArgs::new();
                    args.set("host", host);
                    thinkterm_i18n::tr_args("web-card-busy-on", &args)
                },
                hint: tr("web-card-busy-hint"),
                state: "busy",
                action: taking,
            },
            (_, false) => Card {
                title: tr("web-card-free-title"),
                hint: tr("web-card-free-hint"),
                state: "free",
                action: tr("web-card-take-control"),
            },
        })
    }

    /// Tell the display layer that a view changed: once per task, so a
    /// burst of changes is one notice. A task rather than an animation
    /// frame, since the page's frames are the terminal's, and a notice
    /// that waited for one would be counted -- and paced -- as a paint.
    fn notify(inner: &Inner) {
        let Some(cb) = inner.on_change.clone() else {
            return;
        };
        if inner.notify_pending.replace(true) {
            return;
        }
        let pending = Rc::clone(&inner.notify_pending);
        let tick = Closure::once_into_js(move || {
            pending.set(false);
            if let Err(err) = cb.call0(&JsValue::NULL) {
                log::warn!("the page's change handler failed: {err:?}");
            }
        });
        let queued = web_sys::window()
            .map(|w| w.set_timeout_with_callback_and_timeout_and_arguments_0(tick.as_ref().unchecked_ref(), 0).is_ok())
            .unwrap_or(false);
        if !queued {
            inner.notify_pending.set(false);
        }
    }

    /// The language changed: every view carries text, so all are stale.
    pub fn locale_changed(&self) {
        let inner = self.inner.borrow();
        Self::render_strip(&inner);
    }

    pub fn set_on_change(&self, cb: js_sys::Function) {
        let mut inner = self.inner.borrow_mut();
        inner.on_change = Some(cb);
        drop(inner);
        Self::notify(&self.inner.borrow());
    }

    pub fn sidebar_view(&self) -> crate::views::SidebarView {
        let inner = self.inner.borrow();
        crate::views::SidebarView {
            rows: Self::side_rows(&inner),
            editing: inner.editing.clone(),
            space: inner.tree.current_space().map(|s| s.id.clone()),
            new_project_error: inner.new_project_error.clone(),
            reveal: crate::sidebar::REVEAL,
            footer: Self::side_footer(),
            footer_label_min_width: crate::sidebar::FOOTER_LABEL_MIN_WIDTH,
        }
    }

    /// The footer the desktop's sidebar ends with: the gear and its label,
    /// then Live Overview, Remote Hosts and the thread search. The first
    /// two need windows the browser has not got, so they are shown and
    /// refused rather than hidden -- the panel is the shape the desktop's
    /// is either way.
    fn side_footer() -> Vec<crate::sidebar::FooterAction> {
        // Only what the page can do: the desktop's Live Overview and
        // Remote Hosts have no browser counterpart, so no button for them.
        vec![
            crate::sidebar::FooterAction {
                id: "settings".into(),
                icon: "settings".into(),
                label: Some(thinkterm_i18n::tr("sidebar-settings")),
                tip: thinkterm_i18n::tr("tooltip-sidebar-settings"),
                enabled: true,
                trailing: false,
            },
            crate::sidebar::FooterAction {
                id: "search".into(),
                icon: "search".into(),
                label: None,
                tip: thinkterm_i18n::tr("tooltip-sidebar-thread-search"),
                enabled: true,
                trailing: true,
            },
        ]
    }

    pub fn tabs_view(&self) -> crate::views::TabsView {
        let inner = self.inner.borrow();
        let (tabs, controls) = Self::strip_model(&inner).unwrap_or_default();
        crate::views::TabsView { tabs, controls }
    }

    pub fn navs_view(&self) -> crate::views::NavsView {
        Self::nav_views(&self.inner.borrow())
    }

    pub fn status_view(&self) -> crate::views::StatusView {
        let inner = self.inner.borrow();
        let toast = inner.toast.borrow().clone();
        let summary = inner.summary.borrow().clone();
        crate::views::StatusView { toast, card: Self::card(&inner), summary }
    }

    pub fn layout_view(&self) -> String {
        Self::layout_json(&self.inner.borrow())
    }

    /// The server pushed something for us.
    pub fn on_push(self: &Rc<Self>, pdu: Pdu) {
        let inner = self.inner.borrow();
        match pdu {
            // Every pane on the server pushes to every client; only the
            // ones on this page are wanted, and the rest are simply not
            // ours (never a reason to re-list: there are many).
            Pdu::GetPaneRenderChangesResponse(delta) => {
                if let Some(cell) = inner.panes.get(&delta.pane_id).or_else(|| inner.parked.get(&delta.pane_id)) {
                    cell.session.queue_render_delta(delta);
                }
            }
            Pdu::PaneRemoved(removed) if inner.parked.contains_key(&removed.pane_id) => {
                drop(inner);
                self.inner.borrow_mut().parked.remove(&removed.pane_id);
                // The strip and the sidebar list it still.
                self.refresh_layout_soon();
                self.refresh_session_soon();
            }
            Pdu::PaneRemoved(removed) if inner.panes.contains_key(&removed.pane_id) => {
                let cell = &inner.panes[&removed.pane_id];
                cell.session.set_dead(true);
                if removed.pane_id == inner.focused_pane {
                    Self::set_status(&inner, &tr("web-toast-pane-closed"));
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
                self.refresh_session_soon();
            }
            // Sent once per connection, before the first line change: a
            // program's OSC 4/10/11 colours. Dropping it would leave the
            // page on the stock palette for good.
            Pdu::SetApplicationPalette(codec::SetApplicationPalette { pane_id, palette }) => {
                drop(inner);
                let mut inner = self.inner.borrow_mut();
                inner.overrides.insert(pane_id, palette.clone());
                if let Some(cell) = inner.panes.get_mut(&pane_id) {
                    cell.application = palette;
                    let changed = inner.recompute_palettes();
                    drop(inner);
                    self.repaint_palettes(&changed);
                }
            }
            // The server's own `color_scheme`: the base under any override,
            // unless this browser picked a scheme of its own.
            Pdu::DefaultPalette(codec::DefaultPalette { palette }) => {
                drop(inner);
                let mut inner = self.inner.borrow_mut();
                if inner.server_palette.as_ref() == Some(&palette) {
                    return;
                }
                inner.server_palette = Some(palette);
                let changed = inner.recompute_palettes();
                drop(inner);
                self.repaint_palettes(&changed);
            }
            Pdu::ThinkTermTreeState(state) => {
                drop(inner);
                self.inner.borrow_mut().tree.apply_tree(state.tree);
                // The session view is not pushed with it: ask.
                self.refresh_session_soon();
                Self::notify(&self.inner.borrow());
            }
            Pdu::ThinkTermSessionState(state) => {
                drop(inner);
                let changed = self.inner.borrow_mut().tree.apply_session(state);
                if changed {
                    Self::notify(&self.inner.borrow());
                }
            }
            Pdu::AgentStatusChanged(codec::AgentStatusChanged { pane_id, status }) => {
                drop(inner);
                self.inner.borrow_mut().tree.apply_agent(pane_id, status.as_ref());
                Self::render_strip(&self.inner.borrow());
            }
            Pdu::ClientViewportState(_) | Pdu::FrontendAccessState(_) => {
                drop(inner);
                // The desktop may have resized: the panes are listed again
                // and the page's report follows that listing (a report
                // from the old frames would not compose to the new size);
                // its cell is taken now.
                self.match_desktop_cell();
                self.refresh_layout_soon();
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
            for cell in inner.panes.values().chain(inner.parked.values()) {
                cell.session.set_dead(true);
            }
            // An ask on this socket will not be answered; the card must
            // not sit at "taking" with its button disabled.
            if inner.claim == Claim::Taking {
                inner.claim = Claim::Idle;
                Self::notify(&inner);
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
        let (link, url, token, pane_id, tab_id, size, nav_rows) = {
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
                Self::nav_rows(&inner),
            )
        };
        let outcome = async {
            link.reconnect(&url, &token).await?;
            crate::attach::reattach(&link, tab_id, pane_id, size, nav_rows).await
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
            inner.session_refresh_pending = false;
            for cell in inner.panes.values() {
                cell.session.set_dead(false);
                cell.session.make_all_stale();
            }
            // A parked pane's rows may have moved while the socket was
            // down; it is polled again when it comes back on show.
            for cell in inner.parked.values_mut() {
                cell.session.set_dead(false);
                cell.session.make_all_stale();
                cell.reconnect_stale = true;
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
        drop(lease);
        if cols > inner.cols || rows > inner.rows {
            text.push_str(&format!("  ·  this window fits {}x{}", inner.cols, inner.rows));
        }
        // The one-line summary is for probes and tests; the page shows
        // the card and the strip's hint instead.
        *inner.summary.borrow_mut() = text;
        Self::render_strip(&inner);
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
                self.split(None, thinkterm_proto::SplitDirection::Horizontal);
                return true;
            }
            KeyCode::Char('\\') | KeyCode::Char('|') if ctrl_shift => {
                drop(inner);
                self.split(None, thinkterm_proto::SplitDirection::Vertical);
                return true;
            }
            KeyCode::Char('z') | KeyCode::Char('Z') if ctrl_shift => {
                drop(inner);
                self.toggle_zoom(None);
                return true;
            }
            // Ctrl+Shift+F fits the tab to this window; Ctrl+Shift+T
            // takes the terminal over from another device (Handoff mode).
            KeyCode::Char('f') | KeyCode::Char('F') if ctrl_shift => {
                drop(inner);
                self.fit(true);
                return true;
            }
            // Cmd+= / Cmd+- / Cmd+0 change the terminal's size here, as on
            // the desktop, not the browser's zoom.
            KeyCode::Char('=') | KeyCode::Char('+') if cmd => {
                drop(inner);
                self.step_font(1.0);
                return true;
            }
            KeyCode::Char('-') | KeyCode::Char('_') if cmd => {
                drop(inner);
                self.step_font(-1.0);
                return true;
            }
            KeyCode::Char('0') if cmd => {
                drop(inner);
                self.step_font(0.0);
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
        // After the chords: a key from a page that does not hold the
        // terminal asks for it, like a click on the desktop takes it back,
        // and is typed once the server hands it over.
        if inner.link.lease().needs_claim() {
            inner.after_take_over = Some(AfterTakeOver::Key(key, mods, shift));
            drop(inner);
            self.take_over();
            return true;
        }
        let serial = codec::InputSerial::from_millis(
            thinkterm_session::clock::Clock::wall_millis(&inner.host.clock),
        );
        let mods = KeyModifiers::from_bits_truncate(mods.bits());
        // Typing goes back to following the output.
        let cell = inner.focused_mut();
        cell.scroll_from_bottom = 0;
        cell.scroll_px = 0.0;
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
        cell.scroll_px = 0.0;
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
        cell.scroll_px = 0.0;
        let start = cell.session.paste(text);
        Self::spawn_drain(&cell.session, start);
        drop(inner);
        self.request_frame();
    }

    fn set_cursor(inner: &Inner, cursor: &str) {
        let el: &web_sys::HtmlElement = inner.canvas.as_ref();
        let _ = el.style().set_property("cursor", cursor);
    }

    /// The divider under a point on the canvas, and the point's position
    /// along the divider's axis in device px.
    fn divider_under(inner: &Inner, client_x: f64, client_y: f64) -> Option<(usize, f64)> {
        let layout = inner.tab_layout.as_ref()?;
        let rect = inner.canvas.get_bounding_client_rect();
        let pad = Self::pad(inner);
        let px = (client_x - rect.left()) * inner.dpr - pad.0 as f64;
        let py = (client_y - rect.top()) * inner.dpr - pad.1 as f64;
        let (cw, ch) = (
            inner.glyphs.metrics.cell_size.width as f64,
            inner.glyphs.metrics.cell_size.height as f64,
        );
        let (col, row) = cell_at(px.max(0.0), py.max(0.0), cw, ch);
        layout.dividers.iter().enumerate().find_map(|(i, d)| match *d {
            crate::layout::Divider::Col { col: c, top, rows } if col == c && row >= top && row < top + rows => Some((i, px)),
            crate::layout::Divider::Row { row: r, left, cols } if row == r && col >= left && col < left + cols => Some((i, py)),
            _ => None,
        })
    }

    /// A press on a divider starts dragging it; moves send whole cells
    /// to the server as they accrue; the release ends it. A move over a
    /// divider shows the resize cursor.
    fn divider_pointer(self: &Rc<Self>, ev: &web_sys::PointerEvent, what: Pointer) {
        let (x, y) = (ev.client_x() as f64, ev.client_y() as f64);
        match what {
            Pointer::Down => {
                let mut inner = self.inner.borrow_mut();
                if let Some((idx, at)) = Self::divider_under(&inner, x, y) {
                    inner.drag_divider = Some((idx, at, 0));
                    inner.drag_layout = inner.tab_layout.clone();
                }
            }
            Pointer::Move => {
                let step = {
                    let mut inner = self.inner.borrow_mut();
                    let Some((idx, start, sent)) = inner.drag_divider else {
                        let over = Self::divider_under(&inner, x, y).and_then(|(i, _)| inner.tab_layout.as_ref()?.dividers.get(i).copied());
                        let cursor = match over {
                            Some(crate::layout::Divider::Col { .. }) => "col-resize",
                            Some(crate::layout::Divider::Row { .. }) => "row-resize",
                            None => "default",
                        };
                        Self::set_cursor(&inner, cursor);
                        return;
                    };
                    let rect = inner.canvas.get_bounding_client_rect();
                    let pad = Self::pad(&inner);
                    let divider = inner.tab_layout.as_ref().and_then(|l| l.dividers.get(idx).copied());
                    let (pos, cell) = match divider {
                        Some(crate::layout::Divider::Col { .. }) => (
                            (x - rect.left()) * inner.dpr - pad.0 as f64,
                            inner.glyphs.metrics.cell_size.width as f64,
                        ),
                        Some(crate::layout::Divider::Row { .. }) => (
                            (y - rect.top()) * inner.dpr - pad.1 as f64,
                            inner.glyphs.metrics.cell_size.height as f64,
                        ),
                        None => return,
                    };
                    let cells = ((pos - start) / cell.max(1.0)).round() as isize;
                    if cells == sent {
                        return;
                    }
                    inner.drag_divider = Some((idx, start, cells));
                    (idx, cells - sent, cells)
                };
                // The page's own claim, pane by pane, keeps every bar's rows
                // as the divider moves; a page that does not hold the tab
                // can only ask the server to grow a pane.
                if self.inner.borrow().link.lease().owns_viewport() {
                    self.drag_divider_to(step.0, step.2);
                } else {
                    self.adjust_divider(step.0, step.1);
                }
            }
            Pointer::Up => {
                let was = self.inner.borrow_mut().drag_divider.take();
                self.inner.borrow_mut().drag_layout = None;
                if was.is_some() {
                    // The held-back report goes out with a fresh listing,
                    // and not deduplicated: the drag put every pane at its
                    // whole frame, whatever this page reported last, even
                    // when the divider is back where it started.
                    self.inner.borrow().link.lease_mut().reported_viewport = None;
                    self.refresh_layout();
                }
            }
        }
    }

    /// Keep every drawn pane's rows on screen across a resize the server
    /// has yet to answer: a live-resize preview at the size each pane is
    /// expected to get. Rows are normalised to that width and refetched,
    /// never dropped, until a listing settles the sizes (`settle_previews`).
    fn preview_panes(inner: &mut Inner, sizes: &[(PaneId, TerminalSize)]) {
        use thinkterm_session::lines::FrontendPreviewPolicy;
        inner.preview_epoch += 1;
        let epoch = inner.preview_epoch;
        for (pane, size) in sizes {
            if let Some(cell) = inner.panes.get_mut(pane) {
                cell.session.begin_frontend_preview(epoch, *size, FrontendPreviewPolicy::LiveResize);
                cell.preview = Some((epoch, *size, monotonic_ms()));
            }
        }
    }

    /// The layout with every frame scaled to a `cols` x `rows` grid, edges
    /// rounded so the frames still tile: what the tab looks like once it
    /// takes this window's grid, near enough to claim pane by pane.
    fn scaled_layout(inner: &Inner, cols: usize, rows: usize) -> Option<crate::layout::TabLayout> {
        let layout = inner.tab_layout.as_ref()?;
        if layout.cols == 0 || layout.rows == 0 {
            return None;
        }
        let (cw, ch) = (
            inner.glyphs.metrics.cell_size.width as usize,
            inner.glyphs.metrics.cell_size.height as usize,
        );
        crate::layout::scaled(
            layout,
            TerminalSize {
                cols,
                rows,
                pixel_width: cols * cw,
                pixel_height: rows * ch,
                dpi: (96.0 * inner.dpr) as u32,
            },
        )
    }

    /// The sizes the panes would have if the tab took this grid, scaled
    /// from their frames: near enough for a preview, which only has to
    /// keep rows on screen until the server says the exact size.
    #[allow(dead_code)]
    fn scaled_pane_sizes(inner: &Inner, cols: usize, rows: usize) -> Vec<(PaneId, TerminalSize)> {
        let Some(layout) = inner.tab_layout.as_ref() else {
            return vec![];
        };
        if layout.cols == 0 || layout.rows == 0 {
            return vec![];
        }
        let nav_rows = Self::nav_rows(inner);
        let (cw, ch) = (
            inner.glyphs.metrics.cell_size.width as usize,
            inner.glyphs.metrics.cell_size.height as usize,
        );
        let dpi = (96.0 * inner.dpr) as u32;
        layout
            .panes
            .iter()
            .map(|p| {
                let c = (p.frame.cols * cols + layout.cols / 2) / layout.cols;
                let r = (p.frame.rows * rows + layout.rows / 2) / layout.rows;
                let r = r.saturating_sub(nav_rows).max(1);
                let c = c.max(1);
                (p.pane_id, TerminalSize { cols: c, rows: r, pixel_width: c * cw, pixel_height: r * ch, dpi })
            })
            .collect()
    }

    /// A listing is the server's word on every pane's size: a preview it
    /// confirms ends; one it contradicts is moved to the listed size, rows
    /// kept, so the change never shows as a blank pane.
    /// How long a preview may wait for the server to confirm its size
    /// before the page gives it up and shows what the server has: a
    /// preview holds the pane's rows at a guess and turns away pushes
    /// at any other size, so one the server never meets would leave
    /// the pane painted wrong until something else redrew it.
    const PREVIEW_PATIENCE_MS: f64 = 1_500.0;

    fn settle_previews(inner: &mut Inner) {
        use thinkterm_session::lines::FrontendPreviewPolicy;
        let Some(layout) = inner.tab_layout.clone() else {
            return;
        };
        let now = monotonic_ms();
        for place in &layout.panes {
            let Some(cell) = inner.panes.get_mut(&place.pane_id) else {
                continue;
            };
            let listed = place.size;
            match cell.preview {
                Some((epoch, size, _)) if cell.session.server_geometry_matches(size) => {
                    cell.session.end_frontend_preview(epoch, true);
                    cell.preview = None;
                }
                Some((epoch, _, at)) if now - at > Self::PREVIEW_PATIENCE_MS => {
                    log::info!("pane {}: the server never met the previewed size; showing its own", place.pane_id);
                    cell.session.end_frontend_preview(epoch, false);
                    cell.preview = None;
                }
                Some((_, size, at)) if size != listed => {
                    inner.preview_epoch += 1;
                    let epoch = inner.preview_epoch;
                    cell.session.begin_frontend_preview(epoch, listed, FrontendPreviewPolicy::LiveResize);
                    cell.preview = Some((epoch, listed, at));
                    if cell.session.server_geometry_matches(listed) {
                        cell.session.end_frontend_preview(epoch, true);
                        cell.preview = None;
                    }
                }
                _ => {}
            }
        }
    }

    /// See `Client::panel_drag`. Letting go reshapes the tab to the grid
    /// the canvas ended at, in one claim.
    pub fn set_panel_drag(&self, on: bool) {
        {
            let mut inner = self.inner.borrow_mut();
            if inner.panel_drag == on {
                return;
            }
            inner.panel_drag = on;
            if !on {
                inner.cols = 0;
            }
        }
        if !on {
            self.resize();
        }
    }

    /// Another pane in `pane`'s stack, as the desktop's bar `+` makes;
    /// it is shown and focused.
    pub fn new_in_stack(self: &Rc<Self>, pane: PaneId) {
        self.act(
            "web-act-new-tab",
            Pdu::SpawnPaneInStack(codec::SpawnPaneInStack {
                pane_id: pane,
                command: None,
                command_dir: None,
                domain: thinkterm_proto::SpawnTabDomain::CurrentPaneDomain,
            }),
            |app, answer| {
                if let Pdu::SpawnResponse(spawned) = answer {
                    app.switch_to_pane(spawned.pane_id, false);
                }
            },
        );
    }

    /// Claim the tab with divider `idx` moved `cells` from where the drag
    /// began (right/down positive): every pane touching the divider along
    /// its extent gives or takes the cells, none below one content row.
    fn drag_divider_to(self: &Rc<Self>, idx: usize, cells: isize) {
        let (link, tab_id) = {
            let inner = self.inner.borrow();
            let Some(layout) = inner.drag_layout.as_ref() else {
                return;
            };
            let Some(moved) = crate::layout::with_divider_moved(layout, idx, cells, Self::nav_rows(&inner)) else {
                return;
            };
            let panes = Self::native_panes(&inner, &moved);
            let sizes: Vec<(PaneId, TerminalSize)> = panes.iter().map(|p| (p.pane_id, p.size)).collect();
            let mut lease = inner.link.lease_mut();
            lease.native = panes;
            lease.native_root = Some(moved.size);
            drop(lease);
            let tab_id = layout.tab_id;
            let link = inner.link.clone();
            drop(inner);
            let mut inner = self.inner.borrow_mut();
            Self::preview_panes(&mut inner, &sizes);
            // The picture follows every step; the wire takes one step at
            // a time, and the latest when it is free again.
            if inner.divider_claim_busy {
                inner.divider_claim_next = Some((idx, cells));
                return;
            }
            inner.divider_claim_busy = true;
            (link, tab_id)
        };
        let app = Rc::clone(self);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(err) = link.report_viewport(tab_id).await {
                log::warn!("moving the divider: {err:#}");
            }
            let next = {
                let mut inner = app.inner.borrow_mut();
                inner.divider_claim_busy = false;
                inner.divider_claim_next.take().filter(|_| inner.drag_divider.is_some())
            };
            match next {
                Some((idx, cells)) => app.drag_divider_to(idx, cells),
                None => app.refresh_layout(),
            }
        });
    }

    /// Move divider `idx` by `delta` cells (right/down positive): the
    /// server grows the pane on one side of it in that direction, so the
    /// pane next to the divider is focused first. Nested splits are
    /// approximate: the server picks the nearest split of that
    /// orientation above the pane.
    fn adjust_divider(self: &Rc<Self>, idx: usize, delta: isize) {
        use thinkterm_proto::keyassignment::PaneDirection;
        let (pane, direction, link, palette) = {
            let inner = self.inner.borrow();
            let Some(layout) = inner.tab_layout.as_ref() else {
                return;
            };
            let Some(divider) = layout.dividers.get(idx).copied() else {
                return;
            };
            let pick = |before: bool| -> Option<PaneId> {
                layout
                    .panes
                    .iter()
                    .find(|p| match divider {
                        crate::layout::Divider::Col { col, top, rows } => {
                            let edge = if before { p.frame.left + p.frame.cols == col } else { p.frame.left == col + 1 };
                            edge && p.frame.top < top + rows && p.frame.top + p.frame.rows > top
                        }
                        crate::layout::Divider::Row { row, left, cols } => {
                            let edge = if before { p.frame.top + p.frame.rows == row } else { p.frame.top == row + 1 };
                            edge && p.frame.left < left + cols && p.frame.left + p.frame.cols > left
                        }
                    })
                    .map(|p| p.pane_id)
            };
            let horizontal = matches!(divider, crate::layout::Divider::Col { .. });
            let (pane, direction) = if delta > 0 {
                (pick(true), if horizontal { PaneDirection::Right } else { PaneDirection::Down })
            } else {
                (pick(false), if horizontal { PaneDirection::Left } else { PaneDirection::Up })
            };
            let Some(pane) = pane else {
                return;
            };
            (pane, direction, inner.link.clone(), inner.configured())
        };
        let amount = delta.unsigned_abs();
        let app = Rc::clone(self);
        wasm_bindgen_futures::spawn_local(async move {
            // The server resizes the tab's active pane: make it this one.
            let _ = thinkterm_session::host::request(
                &link,
                Pdu::SetFocusedPane(codec::SetFocusedPane { pane_id: pane, configured_palette: Some(palette) }),
                |p| match p {
                    Pdu::UnitResponse(_) => Ok(()),
                    other => Err(other),
                },
            )
            .await;
            app.focus_pane(pane, false);
            app.act(
                "web-act-resize-pane",
                Pdu::AdjustPaneSize(codec::AdjustPaneSize { pane_id: pane, direction, amount }),
                |_, _| {},
            );
        });
    }

    /// Where a point on the canvas lands: which pane, and the cell within
    /// it. `None` on a divider or past the tab.
    fn hit_under(inner: &Inner, client_x: f64, client_y: f64) -> Option<Hit> {
        let rect = inner.canvas.get_bounding_client_rect();
        let pad = Self::pad(inner);
        let px = (client_x - rect.left()) * inner.dpr - pad.0 as f64;
        let py = (client_y - rect.top()) * inner.dpr - pad.1 as f64;
        if px < 0.0 || py < 0.0 {
            return None;
        }
        let (cw, ch) = (
            inner.glyphs.metrics.cell_size.width as f64,
            inner.glyphs.metrics.cell_size.height as f64,
        );
        let (col, row) = cell_at(px, py, cw, ch);
        let place = match &inner.tab_layout {
            Some(layout) => crate::layout::hit(layout, col, row)?.clone(),
            None => Self::focused_placement(inner)?,
        };
        Some(Self::hit_in(inner, &place, px, py))
    }

    /// The cell of `place` under a canvas point (device px past the
    /// padding), clamped into the pane: a drag that leaves the pane keeps
    /// reporting its edge. The frame is in the page's cells, the cells
    /// within it are the pane's own.
    fn hit_in(inner: &Inner, place: &crate::layout::PanePlacement, px: f64, py: f64) -> Hit {
        let (cols, rows) = Self::shown_in(inner, place);
        let (cw, ch) = Self::pane_cell(inner, place);
        let root = inner.glyphs.metrics.cell_size;
        let nav = Self::content_offset(inner, place) as f64;
        let origin = (place.frame.left as f64 * root.width as f64, place.frame.top as f64 * root.height as f64 + nav);
        // Smooth scrolling draws the rows shifted up by `scroll_px`, with
        // one more row filling the strip that leaves at the bottom; the
        // row under a pixel moves with them, and the extra row is one the
        // pointer can reach. `row` stays an offset from the first row
        // drawn, which is what every caller adds to `visible_rows`'s start.
        let scroll_px = inner
            .panes
            .get(&place.pane_id)
            .map(|cell| (cell.scroll_px as f64).clamp(0.0, ch))
            .unwrap_or(0.0);
        let last = if scroll_px > 0.0 && rows > 0 { rows } else { rows.saturating_sub(1) };
        let local_col = (((px - origin.0) / cw).floor().max(0.0) as usize).min(cols.saturating_sub(1));
        let local_y = py - origin.1 + scroll_px;
        let local_row = ((local_y / ch).floor().max(0.0) as usize).min(last);
        let x_off = (px - origin.0 - local_col as f64 * cw).clamp(0.0, cw) as isize;
        let y_off = (local_y - local_row as f64 * ch).clamp(0.0, ch) as isize;
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
            (inner.link.clone(), inner.configured())
        };
        self.refresh_status();
        self.request_frame();
        if advise {
            Self::advise_focus(link.0, pane_id, link.1);
        }
    }

    /// Tell the server (and so the desktop) which pane has the focus.
    /// Sent and forgotten: the desktop may ignore it for a few seconds
    /// after a focus move of its own, and the echo is the pane already
    /// focused here.
    /// The page's base palette goes with it: the server paints a pane
    /// with the colours of whoever focused it last, as it does for the
    /// desktop, so a pane the page uses shows the page's scheme.
    fn advise_focus(link: crate::link::WsLink, pane_id: PaneId, palette: ColorPalette) {
        wasm_bindgen_futures::spawn_local(async move {
            let pdu = Pdu::SetFocusedPane(codec::SetFocusedPane {
                pane_id,
                configured_palette: Some(palette),
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

    /// Focus the field the terminal types through -- unless the page says
    /// the soft keyboard is not wanted. On a phone (`body[data-mobile]`)
    /// focusing it raises the keyboard over half the screen, so only the
    /// key bar's keyboard button asks for it (`body[data-keyboard]`, kept by
    /// mobile.svelte.ts); a press or a finished rename must not.
    fn focus_terminal(textarea: &web_sys::HtmlElement) {
        if let Some(body) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.body())
        {
            if body.has_attribute("data-mobile") && !body.has_attribute("data-keyboard") {
                return;
            }
        }
        let _ = textarea.focus();
    }

    pub fn pointer(self: &Rc<Self>, ev: &web_sys::PointerEvent, what: Pointer) {
        if what == Pointer::Down {
            // Focus first, outside any borrow: focus() dispatches events
            // synchronously and a listener may look at the app.
            let textarea = self.inner.borrow().textarea.clone();
            Self::focus_terminal(&textarea);
            let canvas = self.inner.borrow().canvas.clone();
            let _ = canvas.set_pointer_capture(ev.pointer_id());
        }
        // A divider under the press is dragged, as on the desktop.
        if what != Pointer::Down && self.inner.borrow().drag_divider.is_some() {
            self.divider_pointer(ev, what);
            return;
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
                        let pad = Self::pad(&inner);
                        let px = (ev.client_x() as f64 - rect.left()) * inner.dpr - pad.0 as f64;
                        let py = (ev.client_y() as f64 - rect.top()) * inner.dpr - pad.1 as f64;
                        Self::hit_in(&inner, &place, px, py)
                    })
                }
                (_, None) => Self::hit_under(&inner, ev.client_x() as f64, ev.client_y() as f64),
            }
        };
        let Some(hit) = hit else {
            self.divider_pointer(ev, what);
            return;
        };
        if what == Pointer::Move {
            Self::set_cursor(&self.inner.borrow(), "text");
        }
        if what == Pointer::Down {
            let (advise, claim) = {
                let inner = self.inner.borrow();
                let claim = inner.link.lease().needs_claim();
                (inner.following, claim)
            };
            if claim {
                // The press takes the terminal over; the desktop is told
                // about the focus once the server has handed it over.
                self.focus_pane(hit.pane_id, false);
                self.inner.borrow_mut().after_take_over =
                    advise.then_some(AfterTakeOver::Focus(hit.pane_id));
                self.take_over();
                return;
            }
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
            .map(|p| Self::shown_in(&inner, &p).1)
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
    pub fn wheel(self: &Rc<Self>, ev: &web_sys::WheelEvent) -> bool {
        let mut inner = self.inner.borrow_mut();
        if inner.disconnected.is_some() {
            return false;
        }
        let Some(hit) = Self::hit_under(&inner, ev.client_x() as f64, ev.client_y() as f64) else {
            return false;
        };
        // Scrolling asks for the terminal like a click does; this notch
        // is spent on that.
        if inner.link.lease().needs_claim() {
            inner.after_take_over = inner.following.then_some(AfterTakeOver::Focus(hit.pane_id));
            drop(inner);
            self.focus_pane(hit.pane_id, false);
            self.take_over();
            return true;
        }
        let place = Self::placements(&inner).into_iter().find(|p| p.pane_id == hit.pane_id);
        let rows_shown = place.as_ref().map(|p| Self::shown_in(&inner, p).1).unwrap_or(inner.rows);
        let cell_h = place
            .as_ref()
            .map(|p| Self::pane_cell(&inner, p).1)
            .unwrap_or(inner.glyphs.metrics.cell_size.height as f64);
        let cell_h_css = cell_h / inner.dpr;
        let lines = match ev.delta_mode() {
            web_sys::WheelEvent::DOM_DELTA_LINE => ev.delta_y(),
            web_sys::WheelEvent::DOM_DELTA_PAGE => ev.delta_y() * rows_shown as f64,
            _ => ev.delta_y() / cell_h_css,
        };
        let notches = lines.abs().round().max(if lines == 0.0 { 0.0 } else { 1.0 }) as usize;
        if notches == 0 {
            return false;
        }
        let smooth = inner.settings.scroll_mode.is_smooth();
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
            // A real Ctrl+wheel (a trackpad pinch on the desktop) stays the
            // browser's zoom. The page synthesises one for a two-finger
            // pinch on the canvas (touch.ts), where the browser's zoom is
            // switched off; that one changes the pane's font instead.
            if ev.is_trusted() {
                return false;
            }
            let pane_id = hit.pane_id;
            let step = if ev.delta_y() < 0.0 { 1.0 } else { -1.0 };
            drop(inner);
            self.focus_pane(pane_id, false);
            self.step_font(step);
            return true;
        }
        let max = max_scroll(&session.dimensions());
        if smooth {
            // Down the page (a positive delta) is towards the newest row,
            // which is the way `normalize_scroll_px` counts. A trackpad or
            // a finger hands over a fraction of a row at a time and keeps
            // it: nothing is rounded up to a notch.
            let (scroll, px) = crate::viewport::normalize_scroll_px(
                cell.scroll_from_bottom,
                cell.scroll_px,
                (lines * cell_h) as f32,
                cell_h as f32,
                max,
            );
            cell.scroll_from_bottom = scroll;
            cell.scroll_px = px;
        } else {
            cell.scroll_from_bottom = if lines < 0.0 {
                (cell.scroll_from_bottom + notches).min(max)
            } else {
                cell.scroll_from_bottom.saturating_sub(notches)
            };
            cell.scroll_px = 0.0;
        }
        drop(inner);
        self.request_frame();
        true
    }

    pub fn focus(&self, focused: bool) {
        self.inner.borrow_mut().focused = focused;
        self.request_frame();
    }

    /// What the page has laid out, as JSON on the canvas element for the
    /// smoke tests and anyone else curious: `canvas.dataset.layout`.
    /// Rewritten whenever the strip is, which is whenever it changes.
    fn publish_layout(inner: &Inner) {
        let json = Self::layout_json(inner);
        let _ = inner.canvas.set_attribute("data-layout", &json);
    }

    /// The layout as a probe reads it.
    fn layout_json(inner: &Inner) -> String {
        let lease = inner.link.lease();
        let dpr = inner.dpr.max(0.1);
        let (cw, ch) = (
            inner.glyphs.metrics.cell_size.width as f64 / dpr,
            inner.glyphs.metrics.cell_size.height as f64 / dpr,
        );
        // The desktop's cell in CSS px, from the canonical size's pixels.
        let desktop_cell = lease
            .canonical_size
            .filter(|s| s.rows > 0 && s.dpi > 0)
            .map(|s| s.pixel_height as f64 / s.rows as f64 * 96.0 / s.dpi as f64)
            .unwrap_or(0.0);
        let mut json = format!(
            "{{\"tab\":{},\"canvas\":[{},{}],\"cell\":[{cw:.3},{ch:.3}],\"font_pt\":{},\"desktop_cell\":{desktop_cell:.3},\"focused\":{},\"owner\":{},\"may_type\":{},\"fit\":{},\"following\":{}",
            inner.tab_id,
            inner.cols,
            inner.rows,
            inner.glyphs.size_pt,
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
                    let shown = Self::shown_in(inner, p);
                    json.push_str(&format!(
                        "{{\"id\":{},\"left\":{},\"top\":{},\"cols\":{},\"rows\":{},\"content\":[{},{}],\"shown\":[{},{}]}}",
                        p.pane_id, p.frame.left, p.frame.top, p.frame.cols, p.frame.rows, p.content.0, p.content.1, shown.0, shown.1
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
        json
    }

    /// Draw the tab strip from what the page knows.
    /// The desktop's cell in CSS px, from the canonical size's pixels.
    fn desktop_cell_css(inner: &Inner) -> Option<f64> {
        inner
            .link
            .lease()
            .canonical_size
            .filter(|s| s.rows > 0 && s.dpi > 0)
            .map(|s| s.pixel_height as f64 / s.rows as f64 * 96.0 / s.dpi as f64)
    }

    /// The bar above each pane, in CSS px.
    fn nav_css(inner: &Inner) -> f64 {
        let cell_css = inner.glyphs.metrics.cell_size.height as f64 / inner.dpr.max(0.1);
        crate::navbar::nav_css(cell_css, Self::desktop_cell_css(inner))
    }

    /// The bar's height as the grid pays for it: whole rows. The bar's
    /// own height is a fraction of a row, and the pane gives up the
    /// rounded-up count; drawing the content a fraction below the bar
    /// left the difference as a blank strip under the last row, on top of
    /// the half-cell pad -- a row and a half of nothing at the bottom of
    /// every pane. The bar is drawn to the rounded height (`nav_views`), so
    /// bar and content meet.
    fn nav_dev(inner: &Inner) -> f32 {
        Self::nav_rows(inner) as f32 * inner.glyphs.metrics.cell_size.height as f32
    }

    /// Where the grid starts in the canvas, in device px: the desktop's
    /// window padding of a cell left and right and half a cell top and
    /// bottom.
    fn pad(inner: &Inner) -> (f32, f32) {
        let cw = inner.glyphs.metrics.cell_size.width as f32;
        let ch = inner.glyphs.metrics.cell_size.height as f32;
        if inner.mobile {
            // A phone has no room to give the grid a margin above and
            // below, and a gap under the last row reads as a cut-off
            // prompt: the rows sit flush with the bottom of the canvas,
            // and whatever part of a row the height cannot fit is the gap
            // under the tab row instead, where it reads as a margin.
            let rows = (inner.canvas.height() as f32 / ch.max(1.0)).floor();
            return (cw, inner.canvas.height() as f32 - rows * ch);
        }
        (cw, ch / 2.0)
    }

    /// What the grid's rows cannot use of the canvas's height: the pad
    /// above and below on a desktop, nothing on a phone (the leftover is
    /// the pad there, see `pad`).
    fn pad_y_total(inner: &Inner) -> u32 {
        if inner.mobile {
            0
        } else {
            (2.0 * Self::pad(inner).1) as u32
        }
    }

    /// Rows of a frame the bar takes from the pane.
    fn nav_rows(inner: &Inner) -> usize {
        crate::navbar::nav_rows(Self::nav_css(inner) * inner.dpr, inner.glyphs.metrics.cell_size.height as f64)
    }

    /// One bar per drawn pane, as the page shows them.
    fn nav_views(inner: &Inner) -> Vec<crate::navbar::NavView> {
        let mut views = Vec::new();
        if let Some(layout) = &inner.tab_layout {
            let dpr = inner.dpr.max(0.1);
            let cell_css = (
                inner.glyphs.metrics.cell_size.width as f64 / dpr,
                inner.glyphs.metrics.cell_size.height as f64 / dpr,
            );
            let closing = inner
                .closing_since
                .is_some_and(|since| monotonic_ms() - since < CLOSE_CONFIRM_MS);
            let pad = Self::pad(inner);
            let pad_css = (pad.0 as f64 / dpr, pad.1 as f64 / dpr);
            let width_css = inner.canvas.width() as f64 / dpr;
            let nav_rows_css = Self::nav_rows(inner) as f64 * cell_css.1;
            for (rect, place) in crate::navbar::rects(layout, cell_css, nav_rows_css, pad_css, width_css)
                .into_iter()
                .zip(&layout.panes)
            {
                let members = place
                    .stack
                    .iter()
                    .map(|m| {
                        // The page's own copy of a drawn pane's title is fresher.
                        let raw = inner
                            .panes
                            .get(&m.pane_id)
                            .map(|c| c.title.as_str())
                            .unwrap_or(&m.title);
                        let (title, busy) = crate::navbar::display_title(raw);
                        let busy = busy || inner.tree.agents.get(&m.pane_id) == Some(&thinkterm_proto::AgentState::Working);
                        crate::navbar::CapsuleView {
                            pane_id: m.pane_id,
                            title: title.to_string(),
                            busy,
                            current: m.pane_id == place.pane_id,
                        }
                    })
                    .collect();
                let focused = place.pane_id == inner.focused_pane;
                views.push(crate::navbar::NavView {
                    rect,
                    members,
                    focused,
                    zoomed: layout.zoomed.is_some(),
                    closing: closing && focused,
                });
            }
        }
        views
    }

    /// The sidebar's rows from the model, with the pending delete shown.
    fn side_rows(inner: &Inner) -> Vec<crate::tree::Row> {
        let deleting = inner
            .deleting
            .as_ref()
            .filter(|(_, since)| monotonic_ms() - since < CLOSE_CONFIRM_MS)
            .map(|(id, _)| id.as_str());
        inner.tree.rows_with(inner.tab_id, &inner.workspace, inner.window_id, deleting)
    }

    /// Ask the server for its tree, its session view and the agents in
    /// its panes: at the start, and again after a reconnect.
    pub fn fetch_tree(self: &Rc<Self>) {
        let link = self.inner.borrow().link.clone();
        let app = Rc::clone(self);
        wasm_bindgen_futures::spawn_local(async move {
            let tree = thinkterm_session::host::request(&link, Pdu::GetThinkTermTree(codec::GetThinkTermTree {}), |p| match p {
                Pdu::ThinkTermTreeState(s) => Ok(s.tree),
                other => Err(other),
            })
            .await;
            match tree {
                Ok(tree) => app.inner.borrow_mut().tree.apply_tree(tree),
                Err(err) => log::warn!("fetching the tree: {err:#}"),
            }
            let agents = thinkterm_session::host::request(&link, Pdu::GetAgentStatuses(codec::GetAgentStatuses {}), |p| match p {
                Pdu::GetAgentStatusesResponse(r) => Ok(r.statuses),
                other => Err(other),
            })
            .await;
            match agents {
                Ok(entries) => {
                    let mut inner = app.inner.borrow_mut();
                    inner.tree.agents.clear();
                    for e in entries {
                        inner.tree.apply_agent(e.pane_id, Some(&e.status));
                        inner.tree.record_agent_details(e.pane_id, &e.status.agent_id, &e.title);
                    }
                }
                Err(err) => log::warn!("fetching agent statuses: {err:#}"),
            }
            app.fetch_session().await;
            Self::render_strip(&app.inner.borrow());
        });
    }

    async fn fetch_session(self: &Rc<Self>) {
        let link = self.inner.borrow().link.clone();
        let state = thinkterm_session::host::request(&link, Pdu::GetThinkTermSessionState(codec::GetThinkTermSessionState {}), |p| match p {
            Pdu::ThinkTermSessionState(s) => Ok(s),
            other => Err(other),
        })
        .await;
        match state {
            Ok(state) => {
                self.inner.borrow_mut().tree.apply_session(state);
                Self::notify(&self.inner.borrow());
            }
            Err(err) => log::warn!("fetching the session view: {err:#}"),
        }
    }

    /// The session view is not pushed when the tree or the panes change;
    /// it is asked for, a moment after the last such push.
    fn refresh_session_soon(self: &Rc<Self>) {
        {
            let mut inner = self.inner.borrow_mut();
            if inner.session_refresh_pending || inner.disconnected.is_some() {
                return;
            }
            inner.session_refresh_pending = true;
        }
        let app = Rc::clone(self);
        let closure = Closure::once_into_js(move || {
            app.inner.borrow_mut().session_refresh_pending = false;
            let app = Rc::clone(&app);
            wasm_bindgen_futures::spawn_local(async move { app.fetch_session().await });
        });
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(closure.as_ref().unchecked_ref(), 150);
        }
    }

    /// A click in the sidebar.
    pub fn on_side_click(self: &Rc<Self>, click: crate::sidebar::SideClick) {
        use crate::sidebar::SideClick;
        if !matches!(click, SideClick::Delete(_)) {
            self.inner.borrow_mut().deleting = None;
        }
        if !matches!(click, SideClick::RenameThread(_) | SideClick::RenameProject(_) | SideClick::NewProject)
            && self.inner.borrow().editing != crate::sidebar::Editing::None
        {
            let inner = &mut *self.inner.borrow_mut();
            inner.editing = crate::sidebar::Editing::None;
            inner.new_project_error = None;
        }
        match click {
            SideClick::Thread(id) => self.activate_thread(id),
            SideClick::Window(window_id) => {
                let pane = {
                    let inner = self.inner.borrow();
                    // The tab this page last showed in that window, else its first.
                    inner.layout.as_ref().and_then(|l| {
                        let tabs: Vec<_> = l
                            .tabs
                            .iter()
                            .filter_map(|t| Some((t.window_and_tab_ids()?, t)))
                            .filter(|((w, _), _)| *w == window_id)
                            .collect();
                        let recent = inner
                            .recent_tabs
                            .iter()
                            .find_map(|r| tabs.iter().find(|((_, id), _)| id == r));
                        recent
                            .or(tabs.first())
                            .and_then(|(_, t)| crate::chrome::active_pane(t))
                            .map(|e| e.pane_id)
                    })
                };
                if let Some(pane) = pane {
                    self.switch_to_pane(pane, true);
                }
            }
            SideClick::ToggleProject(id) => {
                let inner = &mut *self.inner.borrow_mut();
                if !inner.tree.collapsed.remove(&id) {
                    inner.tree.collapsed.insert(id);
                }
                Self::notify(inner);
            }
            SideClick::ToggleArchived => {
                let inner = &mut *self.inner.borrow_mut();
                inner.tree.archived_open = !inner.tree.archived_open;
                Self::notify(inner);
            }
            other => self.on_side_write(other),
        }
    }

    fn now_secs() -> i64 {
        (js_sys::Date::now() / 1000.0) as i64
    }

    fn new_id(kind: &str) -> String {
        crate::tree::new_id(kind, || (js_sys::Math::random() * u32::MAX as f64) as u32)
    }

    /// Send a write, take the tree that comes back, check the intent
    /// against it (a refusal is a remark, never an error), then ask for
    /// the session view the server does not push, and go on.
    fn mutate(self: &Rc<Self>, intent: crate::tree::Intent, then: impl FnOnce(&Rc<Self>) + 'static) {
        self.mutate_or(intent, then, |app, why| Self::set_status(&app.inner.borrow(), &why));
    }

    /// `mutate`, with the refusal handed to a caller that shows it itself
    /// rather than letting it pass by as a remark.
    fn mutate_or(
        self: &Rc<Self>,
        intent: crate::tree::Intent,
        then: impl FnOnce(&Rc<Self>) + 'static,
        refused: impl FnOnce(&Rc<Self>, String) + 'static,
    ) {
        let link = self.inner.borrow().link.clone();
        let app = Rc::clone(self);
        wasm_bindgen_futures::spawn_local(async move {
            let tree = thinkterm_session::host::request(
                &link,
                Pdu::MutateThinkTermTree(codec::MutateThinkTermTree { ops: intent.ops }),
                |p| match p {
                    Pdu::ThinkTermTreeState(s) => Ok(s.tree),
                    other => Err(other),
                },
            )
            .await;
            let tree = match tree {
                Ok(tree) => tree,
                Err(err) => {
                    refused(&app, format!("the server refused: {err:#}"));
                    return;
                }
            };
            let verdict = intent.expect.check(&tree);
            app.inner.borrow_mut().tree.apply_tree(tree);
            app.fetch_session().await;
            match verdict {
                Ok(()) => then(&app),
                Err(why) => refused(&app, why),
            }
        });
    }

    /// Every pane a thread has, for ending its programs.
    fn thread_panes(inner: &Inner, id: &str) -> Vec<PaneId> {
        inner
            .tree
            .thread(id)
            .map(|t| t.tabs.iter().flat_map(|tab| tab.pane_ids.iter().copied()).collect())
            .unwrap_or_default()
    }

    fn kill_panes(self: &Rc<Self>, panes: Vec<PaneId>) {
        for pane_id in panes {
            self.act("web-act-close-pane", Pdu::KillPane(codec::KillPane { pane_id }), |_, _| {});
        }
    }

    /// The sidebar's writes: the same ops the desktop sends.
    fn on_side_write(self: &Rc<Self>, click: crate::sidebar::SideClick) {
        use crate::sidebar::{Editing, SideClick};
        if !matches!(click, SideClick::RenameThread(_) | SideClick::RenameProject(_) | SideClick::NewProject) {
            let inner = &mut *self.inner.borrow_mut();
            inner.editing = Editing::None;
            inner.new_project_error = None;
        }
        match click {
            SideClick::NewThread(project) => {
                let (project, size) = {
                    let inner = self.inner.borrow();
                    let project = project.or_else(|| inner.tree.default_project(inner.tab_id, &inner.workspace));
                    let size = inner.link.lease().canonical_size.unwrap_or_default();
                    (project, size)
                };
                match project {
                    Some(project) => {
                        let id = Self::new_id("thread");
                        let intent = crate::tree::create_thread(&project, id.clone(), Self::now_secs());
                        self.mutate(intent, move |app| app.activate_thread(id));
                    }
                    None => {
                        // No project yet: the server's landing thread
                        // (Default / Home / main) is the first one.
                        let app = Rc::clone(self);
                        wasm_bindgen_futures::spawn_local(async move {
                            let link = app.inner.borrow().link.clone();
                            let ensured = thinkterm_session::host::request(
                                &link,
                                Pdu::EnsureThinkTermThread(codec::EnsureThinkTermThread { preferred_thread_id: None, size }),
                                |p| match p {
                                    Pdu::EnsureThinkTermThreadResponse(r) => Ok(r),
                                    other => Err(other),
                                },
                            )
                            .await;
                            match ensured {
                                Ok(r) => {
                                    app.fetch_tree();
                                    app.activate_thread(r.thread_id);
                                }
                                Err(err) => Self::set_status(&app.inner.borrow(), &failed("web-act-start-thread", &err)),
                            }
                        });
                    }
                }
            }
            SideClick::NewProject => {
                let inner = &mut *self.inner.borrow_mut();
                inner.editing = Editing::NewProject;
                inner.new_project_error = None;
                Self::notify(inner);
            }
            SideClick::RenameThread(id) => {
                let inner = &mut *self.inner.borrow_mut();
                inner.editing = Editing::Thread(id);
                Self::notify(inner);
            }
            SideClick::RenameProject(id) => {
                let inner = &mut *self.inner.borrow_mut();
                inner.editing = Editing::Project(id);
                Self::notify(inner);
            }
            SideClick::Pin(id, on) => {
                self.mutate(crate::tree::set_pinned(&id, on, Self::now_secs()), |_| {});
            }
            SideClick::Delete(id) => {
                // On the first press, as the desktop's menu item is: no
                // second press to confirm.
                let confirmed = {
                    self.inner.borrow_mut().deleting = None;
                    true
                };
                if confirmed {
                    let panes = Self::thread_panes(&self.inner.borrow(), &id);
                    self.kill_panes(panes);
                    self.mutate(crate::tree::delete_thread(&id), |_| {});
                }
            }
            SideClick::Archive(id) => {
                let panes: Vec<PaneId> = {
                    let inner = self.inner.borrow();
                    inner
                        .tree
                        .session
                        .as_ref()
                        .and_then(|s| s.projects.iter().find(|p| p.id == id))
                        .map(|p| p.threads.iter().flat_map(|t| t.tabs.iter()).flat_map(|t| t.pane_ids.iter().copied()).collect())
                        .unwrap_or_default()
                };
                // The flag lands first; the programs end once it has.
                self.mutate(crate::tree::archive_project(&id, true, Self::now_secs()), move |app| app.kill_panes(panes));
            }
            SideClick::Unarchive(id) => {
                self.mutate(crate::tree::archive_project(&id, false, Self::now_secs()), |_| {});
            }
            // The page opens the Space menu itself (`context_menu("space")`).
            SideClick::SpaceMenu => {}
            _ => {}
        }
    }

    /// A key in the sidebar's text field: Enter commits, Escape cancels.
    pub fn on_side_key(self: &Rc<Self>, key: &str, value: String) {
        use crate::sidebar::Editing;
        let editing = self.inner.borrow().editing.clone();
        match key {
            "Escape" => {
                let inner = &mut *self.inner.borrow_mut();
                inner.editing = Editing::None;
                inner.new_project_error = None;
                Self::notify(inner);
                Self::focus_terminal(&inner.textarea);
            }
            "Enter" if editing == Editing::NewProject => self.add_workspace(value.trim()),
            "Enter" => {
                {
                    let inner = &mut *self.inner.borrow_mut();
                    inner.editing = Editing::None;
                    Self::notify(inner);
                    Self::focus_terminal(&inner.textarea);
                }
                let value = value.trim().to_string();
                if value.is_empty() {
                    return;
                }
                match editing {
                    Editing::Thread(id) => self.mutate(crate::tree::rename_thread(&id, &value, Self::now_secs()), |_| {}),
                    Editing::Project(id) => self.mutate(crate::tree::rename_project(&id, &value), |_| {}),
                    Editing::Space(id) => self.mutate(crate::tree::rename_space(&id, &value), |_| {}),
                    Editing::NewProject | Editing::None => {}
                }
            }
            _ => {}
        }
    }

    /// Keep the "Add workspace" field open with `why` beside it, so the
    /// path that was refused can be corrected rather than retyped.
    fn refuse_workspace(self: &Rc<Self>, why: String) {
        let inner = &mut *self.inner.borrow_mut();
        inner.editing = crate::sidebar::Editing::NewProject;
        inner.new_project_error = Some(why);
        Self::notify(inner);
    }

    /// A path typed into "Add workspace". An empty or relative one is
    /// refused here; a directory the tree already has is opened rather
    /// than added twice, as the desktop's `create_project_from_path`
    /// does; anything else is a project and its `main` thread.
    fn add_workspace(self: &Rc<Self>, path: &str) {
        if path.is_empty() {
            self.refuse_workspace(thinkterm_i18n::tr("web-add-workspace-empty"));
            return;
        }
        if !(path.starts_with('/') || path.starts_with('~')) {
            self.refuse_workspace(thinkterm_i18n::tr("web-add-workspace-relative"));
            return;
        }
        let trimmed = path.trim_end_matches('/');
        let (space, existing) = {
            let inner = self.inner.borrow();
            let space = inner.tree.current_space().map(|s| s.id.clone());
            let existing = inner.tree.session.as_ref().and_then(|s| {
                s.projects
                    .iter()
                    .filter(|p| space.as_deref().is_none_or(|id| p.space_id == id))
                    .find(|p| p.path.trim_end_matches('/') == trimmed && !trimmed.is_empty())
                    .and_then(|p| p.threads.first())
                    .map(|t| t.id.clone())
            });
            (space, existing)
        };
        {
            let inner = &mut *self.inner.borrow_mut();
            inner.editing = crate::sidebar::Editing::None;
            inner.new_project_error = None;
            Self::notify(inner);
            Self::focus_terminal(&inner.textarea);
        }
        if let Some(thread) = existing {
            self.activate_thread(thread);
            return;
        }
        let project = Self::new_id("project");
        let thread = Self::new_id("thread");
        let intent = crate::tree::create_project(space.as_deref(), project, thread.clone(), path, Self::now_secs());
        self.mutate_or(
            intent,
            move |app| app.activate_thread(thread),
            |app, why| app.refuse_workspace(why),
        );
    }

    /// Show a thread: its active tab when it has terminals, else have the
    /// server make them (`EnsureThinkTermThread`) and show what it made.
    /// Either way the thread is touched and read, as the desktop does.
    pub fn activate_thread(self: &Rc<Self>, id: String) {
        let (live_tab, size) = {
            let inner = self.inner.borrow();
            // A thread the model has not seen yet (just created) is
            // opened like a cold one: the server knows it.
            let tab = inner.tree.thread(&id).and_then(|thread| {
                inner
                    .recent_tabs
                    .iter()
                    .find(|recent| thread.tabs.iter().any(|t| t.tab_id == **recent))
                    .copied()
                    .or_else(|| thread.tabs.iter().find(|t| t.is_active).or(thread.tabs.first()).map(|t| t.tab_id))
            });
            let size = inner
                .link
                .lease()
                .canonical_size
                .or_else(|| inner.tab_layout.as_ref().map(|l| l.size))
                .unwrap_or_default();
            (tab, size)
        };
        let app = Rc::clone(self);
        wasm_bindgen_futures::spawn_local(async move {
            let link = app.inner.borrow().link.clone();
            let mut workspace = None;
            if live_tab.is_none() {
                let ensured = thinkterm_session::host::request(
                    &link,
                    Pdu::EnsureThinkTermThread(codec::EnsureThinkTermThread {
                        preferred_thread_id: Some(id.clone()),
                        size,
                    }),
                    |p| match p {
                        Pdu::EnsureThinkTermThreadResponse(r) => Ok(r),
                        other => Err(other),
                    },
                )
                .await;
                match ensured {
                    Ok(r) => workspace = Some(r.workspace),
                    Err(err) => {
                        log::warn!("opening thread {id}: {err:#}");
                        Self::set_status(&app.inner.borrow(), &failed("web-act-open-thread", &err));
                        return;
                    }
                }
            }
            let Some(list) = app.list_panes().await else {
                return;
            };
            let pane = list
                .tabs
                .iter()
                .filter(|t| match (&live_tab, &workspace) {
                    (Some(tab), _) => t.window_and_tab_ids().is_some_and(|(_, id)| id == *tab),
                    (None, Some(ws)) => crate::layout::leaves(t).first().is_some_and(|e| &e.workspace == ws),
                    _ => false,
                })
                .find_map(crate::chrome::active_pane)
                .map(|e| e.pane_id);
            if let Some(pane) = pane {
                app.show(list, pane).await;
            }
            let now = (js_sys::Date::now() / 1000.0) as i64;
            let _ = thinkterm_session::host::request(
                &link,
                Pdu::MutateThinkTermTree(codec::MutateThinkTermTree {
                    ops: vec![
                        codec::TreeOp::TouchThread { thread_id: id.clone(), at: now },
                        codec::TreeOp::SetThreadUnread { thread_id: id.clone(), unread: false },
                    ],
                }),
                |p| match p {
                    Pdu::ThinkTermTreeState(s) => Ok(s.tree),
                    other => Err(other),
                },
            )
            .await;
            app.fetch_session().await;
        });
    }

    /// Something the chrome shows changed: the probe's attribute is
    /// rewritten and the page is told.
    fn render_strip(inner: &Inner) {
        Self::publish_layout(inner);
        Self::notify(inner);
    }

    /// The strip's tabs and state, once there is a listing.
    fn strip_model(inner: &Inner) -> Option<(Vec<crate::chrome::TabView>, crate::chrome::Controls)> {
        let layout = inner.layout.as_ref()?;
        let tabs = crate::chrome::model(layout, inner.focused_pane, inner.title(), Some(inner.window_id));
        let (cols, rows) = inner
            .tab_layout
            .as_ref()
            .map(|l| (l.cols, l.rows))
            .unwrap_or((inner.cols, inner.rows));
        let controls = crate::chrome::Controls {
            following: inner.following,
            fit: inner.link.lease().fit,
            closing_tab: inner
                .closing_tab
                .filter(|(_, since)| monotonic_ms() - since < CLOSE_CONFIRM_MS)
                .map(|(tab, _)| tab),
            clipped: (cols > inner.cols || rows > inner.rows).then_some((inner.cols, inner.rows)),
        };
        Some((tabs, controls))
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
        let nav_rows = Self::nav_rows(&self.inner.borrow());
        let node = crate::layout::tab_containing(&list, want)
            .or_else(|| list.tabs.iter().find(|t| crate::layout::layout(t).is_some()));
        let lay = |node: &thinkterm_proto::layout::PaneNode| {
            let tab_size = node
                .window_and_tab_ids()
                .and_then(|(_, tab_id)| self.inner.borrow().link.lease().tab_sizes.get(&tab_id).copied());
            crate::layout::layout_in(node, tab_size, nav_rows)
        };
        let Some(layout) = node.and_then(lay) else {
            let mut inner = self.inner.borrow_mut();
            inner.layout = Some(list);
            Self::render_strip(&inner);
            Self::set_status(&inner, &tr("web-toast-no-panes"));
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
                    // Computed before the lease is held: it reads the lease.
                    let panes = Self::native_panes(&self.inner.borrow(), &layout);
                    let mut lease = link.lease_mut();
                    lease.tab_id = Some(layout.tab_id);
                    lease.tab_owner = None;
                    lease.canonical_size = Some(layout.size);
                    lease.reported_viewport = None;
                    lease.native = panes;
                    lease.native_root = Some(layout.size);
                    lease.fit = false;
                }
                link.report_viewport(layout.tab_id).await
            }
            .await;
            let mut inner = self.inner.borrow_mut();
            inner.switching = false;
            if let Err(err) = outcome {
                log::warn!("switching to tab {}: {err:#}", layout.tab_id);
                Self::set_status(&inner, &failed("web-act-switch-tabs", &err));
                return;
            }
        }
        let tab_id = layout.tab_id;
        let (fresh, changed) = {
            let mut inner = self.inner.borrow_mut();
            inner.tree.apply_panes(&list);
            inner.layout = Some(list);
            let before = (inner.focused_pane, inner.tab_layout.clone());
            let fresh = Self::apply_layout(&mut inner, layout, want);
            // What a keystroke would claim follows the drawn layout.
            if let Some(layout) = &inner.tab_layout {
                let panes = Self::native_panes(&inner, layout);
                let mut lease = link.lease_mut();
                lease.native = panes;
                lease.native_root = Some(layout.size);
            }
            let changed = tab_changed
                || !fresh.is_empty()
                || before.0 != inner.focused_pane
                || before.1 != inner.tab_layout;
            if tab_changed {
                inner.selecting = false;
                inner.ime_anchor = None;
                // Every showing claims afresh: the desktop may have taken
                // this tab since the page last had it.
                inner.auto_claimed = None;
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
            self.match_desktop_cell();
            self.resize();
        }
        // What the page would claim follows the listing; the report is
        // skipped when it is what the server already has, and held back
        // while a divider is being dragged (each step would otherwise
        // reshape the panes twice: the server's resize, then ours).
        let dragging = self.inner.borrow().drag_divider.is_some();
        if !dragging {
            if let Err(err) = link.report_viewport(tab_id).await {
                log::warn!("reporting the tab shape: {err:#}");
                Self::set_status(&self.inner.borrow(), &failed("web-act-report-shape", &err));
            }
        }
        // A listing that changed nothing (the timer's, mostly) is not a
        // reason to paint.
        if changed {
            self.refresh_status();
            self.request_frame();
        }
        // The page holds the terminal it shows: taken at this window's
        // shape as soon as a tab is on screen, and taken back by the
        // desktop only when someone interacts there.
        // A showing the page already owns (the report took an ownerless
        // tab) is handled too: the desktop taking it later is not undone
        // by the next listing.
        let claim = {
            let mut inner = self.inner.borrow_mut();
            let showing = inner.auto_claimed != Some(tab_id) && inner.disconnected.is_none();
            if showing {
                inner.auto_claimed = Some(tab_id);
            }
            let lease = inner.link.lease();
            // Handoff is exclusive: a terminal someone holds is taken by a
            // press on the card, never by a page merely opening.
            let held_exclusively = lease.needs_claim() && lease.owner.is_some();
            showing && !lease.owns_viewport() && !held_exclusively
        };
        if claim {
            self.take_over_with(true);
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
            let mut leaving = std::mem::take(&mut inner.panes);
            // A preview in force would turn away every push while the
            // tab is parked; it is over now, the server's size stands.
            for cell in leaving.values_mut() {
                if let Some((epoch, _, _)) = cell.preview.take() {
                    cell.session.end_frontend_preview(epoch, false);
                }
            }
            inner.parked.extend(leaving);
            inner.tab_id = layout.tab_id;
            // Kept for the tabs recently shown; the rest are built again
            // when next shown, as they were the first time.
            let kept: Vec<TabId> = inner.recent_tabs.iter().take(PARKED_TABS).copied().collect();
            inner.parked.retain(|_, cell| kept.contains(&cell.session.remote_tab_id()));
        }
        inner.recent_tabs.retain(|t| *t != layout.tab_id);
        inner.recent_tabs.insert(0, layout.tab_id);
        inner.recent_tabs.truncate(64);
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
            // A parked pane comes back with what it had, unless it has
            // moved to another tab meanwhile: its session is bound to the
            // old tab's lease, so it is built afresh, as a stranger is.
            if let Some(mut cell) = inner.parked.remove(&place.pane_id) {
                if cell.session.remote_tab_id() == layout.tab_id {
                    // Its program may have set or reset its colours while
                    // the tab was away; only `overrides` heard.
                    cell.application = inner.overrides.get(&place.pane_id).cloned().flatten();
                    if std::mem::take(&mut cell.reconnect_stale) {
                        fresh.push(place.pane_id);
                    }
                    inner.panes.insert(place.pane_id, cell);
                    continue;
                }
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
            let mut cell = PaneCell::new(session, &place.title);
            cell.application = inner.overrides.get(&place.pane_id).cloned().flatten();
            inner.panes.insert(place.pane_id, cell);
            fresh.push(place.pane_id);
        }
        // A fresh cell starts on the stock palette; the base and any
        // override it has are folded in before it is first drawn.
        inner.recompute_palettes();
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
        Self::settle_previews(inner);
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
        if !matches!(click, Click::Close | Click::ClosePane(_)) {
            self.inner.borrow_mut().closing_since = None;
        }
        if !matches!(click, Click::CloseTab(_)) {
            self.inner.borrow_mut().closing_tab = None;
        }
        match click {
            Click::Pane(pane_id) => {
                let hidden = self
                    .inner
                    .borrow()
                    .tab_layout
                    .as_ref()
                    .is_some_and(|l| l.hidden.contains(&pane_id));
                if hidden {
                    // A stack member behind the drawn one: the server
                    // brings it to the front, then it is focused.
                    self.act(
                        "web-act-activate-pane",
                        Pdu::ActivatePaneInStack(codec::ActivatePaneInStack { pane_id }),
                        move |app, _| app.switch_to_pane(pane_id, true),
                    );
                } else {
                    self.switch_to_pane(pane_id, true);
                }
            }
            Click::CloseTab(tab_id) => {
                let panes: Vec<PaneId> = {
                    // On the first press, as the desktop's × is.
                    let mut inner = self.inner.borrow_mut();
                    inner.closing_tab = None;
                    inner
                        .layout
                        .as_ref()
                        .map(|l| {
                            l.tabs
                                .iter()
                                .flat_map(crate::layout::leaves)
                                .filter(|e| e.tab_id == tab_id)
                                .map(|e| e.pane_id)
                                .collect()
                        })
                        .unwrap_or_default()
                };
                for pane_id in panes {
                    self.act(
                        "web-act-close-tab",
                        Pdu::KillPane(codec::KillPane { pane_id }),
                        |_, _| {},
                    );
                }
            }
            Click::ClosePane(pane_id) => {
                let (focused, following) = {
                    let inner = self.inner.borrow();
                    (inner.focused_pane, inner.following)
                };
                if pane_id != focused {
                    self.inner.borrow_mut().closing_since = None;
                    self.focus_pane(pane_id, following);
                }
                self.on_chrome_click(Click::Close);
            }
            Click::Follow => {
                let mut inner = self.inner.borrow_mut();
                inner.following = !inner.following;
                Self::render_strip(&inner);
                Self::focus_terminal(&inner.textarea);
            }
            Click::NewTab => self.new_tab(),
            Click::NewInStack(pane) => self.new_in_stack(pane),
            Click::SplitRight(pane) => self.split(pane, thinkterm_proto::SplitDirection::Horizontal),
            Click::SplitBelow(pane) => self.split(pane, thinkterm_proto::SplitDirection::Vertical),
            Click::Zoom(pane) => self.toggle_zoom(pane),
            Click::Close => {
                // On the first press, as the desktop's × is.
                let confirmed = {
                    self.inner.borrow_mut().closing_since = None;
                    true
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
                    Self::set_status(&inner, &failed(what, &err));
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
            "web-act-new-tab",
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
    pub fn split(self: &Rc<Self>, pane: Option<PaneId>, direction: thinkterm_proto::SplitDirection) {
        self.split_at(pane, direction, true);
    }

    /// Split with the new pane after (`second`) or before the target.
    pub fn split_at(self: &Rc<Self>, pane: Option<PaneId>, direction: thinkterm_proto::SplitDirection, second: bool) {
        let pane_id = self.target_pane(pane);
        self.act(
            "web-act-split-pane",
            Pdu::SplitPane(codec::SplitPane {
                pane_id,
                split_request: thinkterm_proto::SplitRequest {
                    direction,
                    target_is_second: second,
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

    /// A dropped tab: put it at `index` among its window's tabs. The
    /// listing that follows redraws the strip; the drop is refused for a
    /// tab the page does not know.
    pub fn move_tab(self: &Rc<Self>, tab: TabId, index: usize) -> bool {
        let window_id = {
            let inner = self.inner.borrow();
            let Some((tabs, _)) = Self::strip_model(&inner) else {
                return false;
            };
            let Some(view) = tabs.iter().find(|t| t.tab_id == tab) else {
                return false;
            };
            if tabs.iter().position(|t| t.tab_id == tab) == Some(index) {
                return true;
            }
            view.window_id
        };
        self.act("web-act-move-tab", Pdu::MoveTab(codec::MoveTab { window_id, tab_id: tab, index }), |_, _| {});
        true
    }

    /// A pane dropped on another: it leaves its place and becomes the
    /// target's neighbour in `direction`, after it (`second`) or before.
    /// Dropping a pane on itself is nothing.
    pub fn move_pane(
        self: &Rc<Self>,
        pane: PaneId,
        target: PaneId,
        direction: thinkterm_proto::SplitDirection,
        second: bool,
    ) -> bool {
        if pane == target {
            return false;
        }
        self.act(
            "web-act-move-pane",
            Pdu::SplitPane(codec::SplitPane {
                pane_id: target,
                split_request: thinkterm_proto::SplitRequest {
                    direction,
                    target_is_second: second,
                    top_level: false,
                    size: thinkterm_proto::SplitSize::Percent(50),
                },
                command: None,
                command_dir: None,
                domain: thinkterm_proto::SpawnTabDomain::CurrentPaneDomain,
                move_pane_id: Some(pane),
            }),
            move |app, _| app.switch_to_pane(pane, false),
        );
        true
    }

    /// A thread dropped among its project's unpinned threads, before
    /// `before` (None: last); refused across projects and for pinned rows.
    pub fn move_thread(self: &Rc<Self>, id: &str, before: Option<&str>) -> bool {
        let intent = {
            let inner = self.inner.borrow();
            let Some(project) = inner.tree.project_of(id) else {
                return false;
            };
            let unpinned: Vec<String> =
                project.threads.iter().filter(|t| !t.is_pinned).map(|t| t.id.clone()).collect();
            if !unpinned.iter().any(|t| t == id) || before.is_some_and(|b| !unpinned.iter().any(|t| t == b)) {
                return false;
            }
            crate::tree::move_thread_before(&project.id, id, before, &unpinned)
        };
        self.mutate(intent, |_| {});
        true
    }

    /// A project dropped among its Space's projects, before `before`
    /// (None: last); refused across Spaces.
    pub fn move_project(self: &Rc<Self>, id: &str, before: Option<&str>) -> bool {
        let intent = {
            let inner = self.inner.borrow();
            let Some(session) = inner.tree.session.as_ref() else {
                return false;
            };
            let Some(space) = session.projects.iter().find(|p| p.id == id).map(|p| p.space_id.clone()) else {
                return false;
            };
            let order: Vec<String> =
                session.projects.iter().filter(|p| p.space_id == space).map(|p| p.id.clone()).collect();
            if before.is_some_and(|b| !order.iter().any(|p| p == b)) {
                return false;
            }
            crate::tree::move_project_before(&space, id, before, &order)
        };
        self.mutate(intent, |_| {});
        true
    }

    /// A context menu for something on the page: `kind` names what was
    /// clicked, `id` which one. Empty when there is nothing to offer.
    pub fn context_menu(&self, kind: &str, id: &str) -> Vec<crate::menu::MenuItem> {
        use crate::menu;
        let inner = self.inner.borrow();
        let menu = match kind {
            "pane" => id.parse().ok().map(|pane| menu::for_pane(pane, inner.link.lease().mode)),
            "tab" => id.parse().ok().and_then(|tab: TabId| {
                let (tabs, _) = Self::strip_model(&inner)?;
                let index = tabs.iter().position(|t| t.tab_id == tab)?;
                Some(menu::for_tab(tab, index, tabs.len(), tabs[index].target))
            }),
            "thread" => Self::side_rows(&inner).into_iter().find_map(|row| match row {
                crate::tree::Row::Thread(t) if t.id == id => Some(menu::for_thread(&t)),
                _ => None,
            }),
            "project" => Some(menu::for_project(id, inner.tree.live_panes_of_project(id).len())),
            "archived-project" => Some(menu::for_archived_project(id, inner.tree.thread_count_of_project(id))),
            "sidebar-options" => Some(menu::for_sidebar_options(inner.tree.archived_open, inner.tree.archived_count())),
            "space" => Some(menu::for_space(&inner.tree.spaces())),
            "notifications" => Some(menu::for_notifications()),
            _ => None,
        };
        menu.unwrap_or_default()
    }

    /// The Agents panel: every pane an agent runs in, and the summary line.
    pub fn agents_view(&self) -> crate::agents::AgentsView {
        let inner = self.inner.borrow();
        let rows = crate::agents::rows(&inner.tree, inner.window_id, |pane| {
            inner.panes.get(&pane).map(|c| crate::navbar::display_title(&c.title).0)
        });
        let summary = crate::agents::summary(&rows);
        let tabs = crate::agents::panel_tabs();
        crate::agents::AgentsView { rows, summary, tabs, active: "agents" }
    }

    /// Bring an agent's pane on show: switch to it here, or open the
    /// thread that holds it when it is in another window.
    pub fn agent_reveal(self: &Rc<Self>, pane: PaneId) -> bool {
        let (here, thread) = {
            let inner = self.inner.borrow();
            let here = inner.tab_layout.as_ref().is_some_and(|l| l.panes.iter().any(|p| p.stack.iter().any(|m| m.pane_id == pane)))
                || inner.layout.as_ref().is_some_and(|l| {
                    l.tabs
                        .iter()
                        .filter(|t| t.window_and_tab_ids().is_some_and(|(w, _)| w == inner.window_id))
                        .any(|t| crate::layout::leaves(t).iter().any(|e| e.pane_id == pane))
                });
            let thread = inner.tree.session.as_ref().and_then(|s| {
                s.projects.iter().flat_map(|p| p.threads.iter()).find(|t| t.tabs.iter().any(|tab| tab.pane_ids.contains(&pane))).map(|t| t.id.clone())
            });
            (here, thread)
        };
        if here {
            self.switch_to_pane(pane, true);
            return true;
        }
        match thread {
            Some(id) => {
                let space = self.inner.borrow().tree.project_of(&id).map(|p| p.space_id.clone());
                if let Some(space) = space {
                    self.set_space(&space);
                }
                self.activate_thread(id);
                true
            }
            None => {
                // A window no thread claims: its listing knows the pane.
                self.switch_to_pane(pane, true);
                true
            }
        }
    }

    /// Re-upload and repaint the panes whose palette moved: every glyph and
    /// the ground behind it are coloured from it.
    fn repaint_palettes(self: &Rc<Self>, changed: &[PaneId]) {
        if changed.is_empty() {
            return;
        }
        {
            let inner = self.inner.borrow();
            for pane_id in changed {
                if let Some(cell) = inner.panes.get(pane_id) {
                    cell.session.make_all_stale();
                }
            }
        }
        self.request_frame();
    }

    /// The colours of the scheme this browser picked, as the page read them
    /// out of `schemes.json`; `None` follows the server's configuration.
    pub fn set_terminal_palette(self: &Rc<Self>, palette: Option<ColorPalette>) {
        let changed = {
            let mut inner = self.inner.borrow_mut();
            if inner.chosen_palette == palette {
                return;
            }
            inner.chosen_palette = palette;
            inner.recompute_palettes()
        };
        self.repaint_palettes(&changed);
        // The pane in use takes the new scheme on the server too.
        let (link, pane, palette) = {
            let inner = self.inner.borrow();
            (inner.link.clone(), inner.focused_pane, inner.configured())
        };
        Self::advise_focus(link, pane, palette);
        Self::notify(&self.inner.borrow());
    }

    /// The page's preferences, as the model holds them.
    pub fn settings_view(&self) -> crate::settings::WebSettings {
        self.inner.borrow().settings.clone()
    }

    /// The page's stored preferences, at boot: applied whole.
    pub fn apply_settings(self: &Rc<Self>, json: &str) -> Result<(), String> {
        let settings = crate::settings::WebSettings::parse(json)?;
        self.inner.borrow_mut().settings = settings.clone();
        self.apply_language(&settings.language);
        self.set_font_mode(settings.font);
        if settings.terminal_scheme == crate::settings::FOLLOW_DESKTOP {
            self.set_terminal_palette(None);
        }
        Ok(())
    }

    /// One preference changed on the page.
    pub fn set_setting(self: &Rc<Self>, key: &str, value: &str) -> Result<(), String> {
        let (language, font, scheme) = {
            let mut inner = self.inner.borrow_mut();
            inner.settings.set(key, value)?;
            (
                inner.settings.language.clone(),
                inner.settings.font,
                inner.settings.terminal_scheme.clone(),
            )
        };
        match key {
            "language" => self.apply_language(&language),
            "font" => self.set_font_mode(font),
            // Back to the desktop's scheme: the colours the page sends with
            // a named pick are simply dropped.
            "terminal-scheme" if scheme == crate::settings::FOLLOW_DESKTOP => {
                self.set_terminal_palette(None)
            }
            // Stepped mode holds no part of a row: whatever a pane was
            // through goes, rather than staying frozen until it is scrolled.
            "scroll-mode" => {
                for cell in self.inner.borrow_mut().panes.values_mut() {
                    cell.scroll_px = 0.0;
                }
                Self::notify(&self.inner.borrow());
                self.request_frame();
            }
            _ => Self::notify(&self.inner.borrow()),
        }
        Ok(())
    }

    fn apply_language(&self, preference: &str) {
        let languages = self.inner.borrow().languages.clone();
        thinkterm_i18n::activate_preference(preference, &languages);
        self.locale_changed();
    }

    /// The base font: a size of the page's own, or the desktop's cell
    /// again. Pinning rasterises at that size and refits the grid.
    pub fn set_font_mode(self: &Rc<Self>, mode: crate::settings::FontMode) {
        use crate::settings::FontMode;
        match mode {
            FontMode::Pinned { pt } => {
                let pt = pt.clamp(6.0, 72.0);
                let mut inner = self.inner.borrow_mut();
                inner.font_pinned = true;
                inner.base_size_pt = pt;
                if (inner.glyphs.size_pt - pt).abs() >= 0.125 {
                    let dpi = (96.0 * inner.dpr) as u32;
                    Self::rerasterise(&mut inner, pt, dpi);
                    inner.cols = 0;
                }
                drop(inner);
                self.resize();
                self.request_frame();
            }
            FontMode::Follow => {
                let mut inner = self.inner.borrow_mut();
                inner.font_pinned = false;
                inner.base_size_pt = inner.boot_size_pt;
                drop(inner);
                self.match_desktop_cell();
                self.request_frame();
            }
        }
    }

    /// What the palette can find, from the model, ranked for `query`.
    pub fn palette(&self, query: &str) -> crate::palette::Results {
        use crate::palette::{path_terms, Entry, Group};
        let inner = self.inner.borrow();
        let mut entries = Vec::new();
        if let Some(session) = &inner.tree.session {
            let space_name = |id: &str| session.spaces.iter().find(|s| s.id == id).map(|s| s.name.clone()).unwrap_or_default();
            for project in &session.projects {
                for thread in &project.threads {
                    let icon = match inner.tree.status_of(thread) {
                        crate::tree::Status::Running => "loader-circle",
                        crate::tree::Status::NeedsAttention => "circle-alert",
                        crate::tree::Status::Done => "circle-check",
                        crate::tree::Status::Idle => "square-terminal",
                    };
                    entries.push(Entry {
                        id: format!("thread:{}", thread.id),
                        title: thread.name.clone(),
                        subtitle: project.name.clone(),
                        icon,
                        accessory: space_name(&project.space_id),
                        group: Group::Threads,
                        terms: path_terms(&project.path),
                    });
                }
            }
        }
        if let Some((tabs, _)) = Self::strip_model(&inner) {
            for tab in tabs {
                entries.push(Entry {
                    id: format!("tab:{}", tab.tab_id),
                    title: tab.label,
                    subtitle: String::new(),
                    icon: "square-terminal",
                    accessory: String::new(),
                    group: Group::Tabs,
                    terms: tab.title,
                });
            }
        }
        if let Some(layout) = &inner.tab_layout {
            for place in &layout.panes {
                for member in &place.stack {
                    let raw = inner.panes.get(&member.pane_id).map(|c| c.title.as_str()).unwrap_or(&member.title);
                    let (title, _) = crate::navbar::display_title(raw);
                    entries.push(Entry {
                        id: format!("pane:{}", member.pane_id),
                        title,
                        subtitle: String::new(),
                        icon: "square-terminal",
                        accessory: String::new(),
                        group: Group::Panes,
                        terms: raw.to_string(),
                    });
                }
            }
        }
        let spaces = inner.tree.spaces();
        if spaces.len() > 1 {
            for space in spaces {
                entries.push(Entry {
                    id: format!("space:{}", space.id),
                    title: space.name,
                    subtitle: String::new(),
                    icon: if space.default { "house" } else { "layers" },
                    accessory: String::new(),
                    group: Group::Spaces,
                    terms: String::new(),
                });
            }
        }
        entries.extend(crate::palette::commands());
        crate::palette::results(query, entries, &inner.recent)
    }

    /// The page's remembered picks, given back at boot.
    pub fn set_recent(&self, ids: Vec<String>) {
        self.inner.borrow_mut().recent = ids;
    }

    /// Do what a palette pick asks. `page` names what only the page can
    /// do (its sidebar, its settings panel, remembering the language).
    pub fn palette_run(self: &Rc<Self>, id: &str) -> crate::views::PaletteOutcome {
        use crate::sidebar::SideClick;
        let mut outcome = crate::views::PaletteOutcome { handled: true, page: None, recent: Vec::new() };
        let (kind, arg) = id.split_once(':').unwrap_or((id, ""));
        match (kind, arg) {
            ("thread", id) => {
                // A thread in another Space: the sidebar follows it there.
                let space = self.inner.borrow().tree.project_of(id).map(|p| p.space_id.clone());
                if let Some(space) = space {
                    self.set_space(&space);
                }
                self.activate_thread(id.to_string());
            }
            ("tab", id) => {
                let target = id.parse().ok().and_then(|tab: TabId| {
                    let inner = self.inner.borrow();
                    Self::strip_model(&inner)?.0.into_iter().find(|t| t.tab_id == tab).map(|t| t.target)
                });
                match target {
                    Some(pane) => self.switch_to_pane(pane, true),
                    None => outcome.handled = false,
                }
            }
            ("pane", id) => match id.parse() {
                Ok(pane) => self.switch_to_pane(pane, true),
                Err(_) => outcome.handled = false,
            },
            ("space", id) => outcome.handled = self.set_space(id),
            ("cmd", "new-thread") => self.on_side_click(SideClick::NewThread(None)),
            ("cmd", "new-tab") => self.new_tab(),
            ("cmd", "split-right") => self.split(None, thinkterm_proto::SplitDirection::Horizontal),
            ("cmd", "split-down") => self.split(None, thinkterm_proto::SplitDirection::Vertical),
            ("cmd", "zoom") => self.toggle_zoom(None),
            ("cmd", "close-pane") => self.close_pane(),
            ("cmd", "take-over") => self.take_over(),
            ("cmd", "follow") => self.on_chrome_click(crate::chrome::Click::Follow),
            ("cmd", "toggle-sidebar") | ("cmd", "settings") => outcome.page = Some(arg.to_string()),
            ("cmd", "font-up") => self.step_font(1.0),
            ("cmd", "font-down") => self.step_font(-1.0),
            ("cmd", "font-reset") => self.step_font(0.0),
            _ => outcome.handled = false,
        }
        if outcome.handled {
            let inner = &mut *self.inner.borrow_mut();
            crate::palette::remember(&mut inner.recent, id);
            outcome.recent = inner.recent.clone();
        }
        outcome
    }

    /// Show a Space in the sidebar; false when the server has no such Space.
    pub fn set_space(&self, id: &str) -> bool {
        let inner = &mut *self.inner.borrow_mut();
        let known = inner.tree.set_space(id);
        if known {
            inner.editing = crate::sidebar::Editing::None;
            inner.new_project_error = None;
            Self::notify(inner);
        }
        known
    }

    /// Every pane of a tab, from the last listing.
    fn tab_panes(inner: &Inner, tab: TabId) -> Vec<PaneId> {
        inner
            .layout
            .as_ref()
            .and_then(|l| l.tabs.iter().find(|t| t.window_and_tab_ids().is_some_and(|(_, id)| id == tab)))
            .map(|node| crate::layout::leaves(node).iter().map(|e| e.pane_id).collect())
            .unwrap_or_default()
    }

    /// Do what a menu row asks. Copy hands the selection back for the
    /// page to put on the clipboard; Paste asks the page to read it.
    pub fn menu_action(self: &Rc<Self>, id: &str) -> crate::views::MenuOutcome {
        use crate::menu::{MenuAction, Side};
        use crate::sidebar::SideClick;
        let mut outcome = crate::views::MenuOutcome { handled: true, copy: None, paste: false };
        let Some(action) = MenuAction::parse(id) else {
            outcome.handled = false;
            return outcome;
        };
        match action {
            MenuAction::Copy => outcome.copy = Self::selection_text(&self.inner.borrow()),
            MenuAction::Paste => outcome.paste = true,
            MenuAction::Split { pane, side } => {
                use thinkterm_proto::SplitDirection::{Horizontal, Vertical};
                let (direction, second) = match side {
                    Side::Right => (Horizontal, true),
                    Side::Left => (Horizontal, false),
                    Side::Down => (Vertical, true),
                    Side::Up => (Vertical, false),
                };
                self.split_at(Some(pane), direction, second);
            }
            MenuAction::FrontendAccess(mode) => {
                let (tab_id, viewport) = {
                    let inner = self.inner.borrow();
                    let viewport = inner.link.lease().claim_viewport();
                    (inner.tab_id, viewport)
                };
                match viewport {
                    Some(viewport) => self.act(
                        "web-act-set-access",
                        Pdu::SetFrontendAccessMode(codec::SetFrontendAccessMode { mode, tab_id, viewport }),
                        |_, _| {},
                    ),
                    None => outcome.handled = false,
                }
            }
            MenuAction::CloseTabsLeft(tab) | MenuAction::CloseTabsRight(tab) | MenuAction::CloseOtherTabs(tab) => {
                let panes: Vec<PaneId> = {
                    let inner = self.inner.borrow();
                    let tabs = Self::strip_model(&inner).map(|(t, _)| t).unwrap_or_default();
                    let at = tabs.iter().position(|t| t.tab_id == tab).unwrap_or(0);
                    tabs.iter()
                        .enumerate()
                        .filter(|(i, t)| match action {
                            MenuAction::CloseTabsLeft(_) => *i < at,
                            MenuAction::CloseTabsRight(_) => *i > at,
                            _ => t.tab_id != tab,
                        })
                        .flat_map(|(_, t)| Self::tab_panes(&inner, t.tab_id))
                        .collect()
                };
                self.kill_panes(panes);
            }
            MenuAction::NewTabRight => self.new_tab(),
            MenuAction::Zoom(pane) => self.toggle_zoom(Some(pane)),
            MenuAction::Pin(id, on) => self.on_side_click(SideClick::Pin(id, on)),
            MenuAction::RenameThread(id) => self.on_side_click(SideClick::RenameThread(id)),
            MenuAction::DeleteThread(id) => {
                // The desktop's menu deletes at once; the two presses are
                // the sidebar button's.
                let panes = Self::thread_panes(&self.inner.borrow(), &id);
                self.kill_panes(panes);
                self.mutate(crate::tree::delete_thread(&id), |_| {});
            }
            MenuAction::MarkUnread(id) => self.mutate(crate::tree::set_unread(&id, true), |_| {}),
            MenuAction::RenameProject(id) => self.on_side_click(SideClick::RenameProject(id)),
            MenuAction::NewThread(id) => self.on_side_click(SideClick::NewThread(Some(id))),
            MenuAction::ToggleCollapsed(id) => self.on_side_click(SideClick::ToggleProject(id)),
            MenuAction::ArchiveProject(id) => self.on_side_click(SideClick::Archive(id)),
            MenuAction::UnarchiveProject(id) => self.on_side_click(SideClick::Unarchive(id)),
            MenuAction::RemoveProject(id) => {
                let panes = self.inner.borrow().tree.live_panes_of_project(&id);
                self.kill_panes(panes);
                self.mutate(crate::tree::remove_project(&id), |_| {});
            }
            MenuAction::ShowArchived => self.on_side_click(SideClick::ToggleArchived),
            MenuAction::SwitchSpace(id) => outcome.handled = self.set_space(&id),
            MenuAction::NewSpace => {
                let id = Self::new_id("space");
                let count = self.inner.borrow().tree.spaces().len();
                let name = format!("Space {}", count + 1);
                let chosen = id.clone();
                self.mutate(crate::tree::create_space(id, &name), move |app| {
                    app.set_space(&chosen);
                });
            }
            MenuAction::RenameSpace(id) => {
                let inner = &mut *self.inner.borrow_mut();
                inner.editing = crate::sidebar::Editing::Space(id);
                Self::notify(inner);
            }
            MenuAction::DeleteSpace(id) => {
                let panes: Vec<PaneId> = {
                    let inner = self.inner.borrow();
                    inner
                        .tree
                        .session
                        .as_ref()
                        .map(|s| {
                            s.projects
                                .iter()
                                .filter(|p| p.space_id == id)
                                .flat_map(|p| inner.tree.live_panes_of_project(&p.id))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                self.kill_panes(panes);
                self.mutate(crate::tree::delete_space(&id), |app| {
                    let first = app.inner.borrow().tree.spaces().first().map(|s| s.id.clone());
                    if let Some(first) = first {
                        app.set_space(&first);
                    }
                });
            }
        }
        outcome
    }

    /// The pane a bar's button or a chord acts on: the bar's pane, which
    /// the page focuses first as the desktop does, else the focused one.
    fn target_pane(self: &Rc<Self>, pane: Option<PaneId>) -> PaneId {
        match pane {
            Some(pane_id) => {
                self.focus_pane(pane_id, true);
                pane_id
            }
            None => self.inner.borrow().focused_pane,
        }
    }

    /// Zoom the focused pane to the whole tab, or back.
    pub fn toggle_zoom(self: &Rc<Self>, pane: Option<PaneId>) {
        let pane_id = self.target_pane(pane);
        let (tab_id, zoomed) = {
            let inner = self.inner.borrow();
            let zoomed = inner.tab_layout.as_ref().is_some_and(|l| l.zoomed.is_some());
            (inner.tab_id, zoomed)
        };
        self.act(
            "web-act-zoom-pane",
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
            "web-act-close-pane",
            Pdu::KillPane(codec::KillPane { pane_id }),
            |_, _| {},
        );
    }

    /// The pane at `pane_id`, and the page's focus with it. `chosen` is a
    /// person's click, which also stops the page following the desktop;
    /// a focus push is not.
    pub fn switch_to_pane(self: &Rc<Self>, pane_id: PaneId, chosen: bool) {
        // `chosen` is a person's click: the desktop is told, as with a
        // click in a pane, so both screens keep one focus.
        let _ = chosen;
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
                    Self::set_status(&inner, &failed("web-act-resize-tab", &err));
                }
            }
        });
    }

    /// Take the terminal from whichever device holds it (Handoff mode).
    /// There is no giving it back: the desktop takes it by interacting.
    pub fn take_over(self: &Rc<Self>) {
        self.take_over_with(false);
    }

    /// `quiet`: the page's own claim on showing a tab, not a person's
    /// action, so a refusal is logged rather than remarked on.
    fn take_over_with(self: &Rc<Self>, quiet: bool) {
        let (link, tab_id) = {
            let mut inner = self.inner.borrow_mut();
            if !quiet {
                // The card says it is on the wire; a toast would sit under
                // the card.
                inner.claim = Claim::Taking;
                Self::notify(&inner);
            }
            // Taken by an interaction here: at this page's own shape.
            inner.link.lease_mut().fit = true;
            (inner.link.clone(), inner.tab_id)
        };
        let app = Rc::clone(self);
        wasm_bindgen_futures::spawn_local(async move {
            let after = match link.claim(tab_id).await {
                Ok(true) => {
                    // Back to the page's own size, and the grid with it.
                    app.match_desktop_cell();
                    app.refresh_status();
                    app.request_frame();
                    let mut inner = app.inner.borrow_mut();
                    inner.claim = Claim::Idle;
                    Self::notify(&inner);
                    inner.after_take_over.take()
                }
                Ok(false) => {
                    let mut inner = app.inner.borrow_mut();
                    inner.after_take_over = None;
                    if quiet {
                        log::info!("the server did not hand the terminal over on showing the tab");
                    } else {
                        inner.claim = Claim::Refused;
                        Self::notify(&inner);
                    }
                    None
                }
                Err(err) => {
                    let mut inner = app.inner.borrow_mut();
                    inner.after_take_over = None;
                    if quiet {
                        log::warn!("claiming the tab on showing it: {err:#}");
                    } else {
                        inner.claim = Claim::Refused;
                        Self::notify(&inner);
                        Self::set_status(&inner, &failed("web-act-take-over", &err));
                    }
                    None
                }
            };
            match after {
                Some(AfterTakeOver::Focus(pane_id)) => {
                    let (link, palette) = {
                        let inner = app.inner.borrow();
                        (inner.link.clone(), inner.configured())
                    };
                    Self::advise_focus(link, pane_id, palette);
                }
                Some(AfterTakeOver::Key(key, mods, shift)) => {
                    app.key_down(key, mods, shift);
                }
                None => {}
            }
        });
    }

    /// See `GlyphCache::warm`. Called once, after the first frame.
    pub fn warm_glyph_canvas(&self) {
        self.inner.borrow_mut().glyphs.warm();
    }

    /// Rasterise for another size or density: a fresh cache on a fresh
    /// texture. A new cell size makes "what fits" a different question,
    /// so the atlas backoff starts over rather than carrying a grudge
    /// from the old one.
    fn rerasterise(inner: &mut Inner, size_pt: f64, dpi: u32) {
        let side = inner.glyphs.atlas.size() as u32;
        match GpuTexture::new(&inner.gpu.device, Arc::clone(&inner.gpu.queue), side, side).and_then(
            |t| {
                GlyphCache::new(
                    Rc::clone(&inner.fonts),
                    size_pt,
                    dpi,
                    Rc::new(t),
                    Rc::clone(&inner.glyphs.families),
                )
            },
        ) {
            Ok(glyphs) => inner.glyphs = glyphs,
            Err(err) => log::error!("glyph cache at {size_pt} pt, {dpi} dpi: {err:#}"),
        }
        inner.capacity.reset();
        Self::rebuild_pane_fonts(inner);
    }

    /// The scaled panes' caches follow the page's size, density and atlas
    /// side; a cache that cannot be built leaves the pane at the page's
    /// size.
    fn rebuild_pane_fonts(inner: &mut Inner) {
        let side = inner.glyphs.atlas.size() as u32;
        let (size_pt, dpi) = (inner.glyphs.size_pt, inner.glyphs.dpi);
        let mut failed = Vec::new();
        for (pane_id, font) in inner.pane_fonts.iter_mut() {
            let built = GpuTexture::new(&inner.gpu.device, Arc::clone(&inner.gpu.queue), side, side).and_then(|t| {
                GlyphCache::new(
                    Rc::clone(&inner.fonts),
                    size_pt * font.scale,
                    dpi,
                    Rc::new(t),
                    Rc::clone(&inner.glyphs.families),
                )
            });
            match built {
                Ok(glyphs) => font.glyphs = glyphs,
                Err(err) => {
                    log::error!("glyph cache for pane {pane_id} at scale {}: {err:#}", font.scale);
                    failed.push(*pane_id);
                }
            }
        }
        for pane_id in failed {
            inner.pane_fonts.remove(&pane_id);
        }
    }

    /// Whether a pane's own font is what the page draws it with. It is
    /// while this page holds the terminal (its claim shapes the pane to
    /// that cell), and otherwise only when the pane, as the server has
    /// it, fits its frame at that cell -- a follower never clips a pane.
    fn pane_font_shown(inner: &Inner, place: &crate::layout::PanePlacement) -> bool {
        let Some(font) = inner.pane_fonts.get(&place.pane_id) else {
            return false;
        };
        if inner.link.lease().owns_viewport() {
            return true;
        }
        let root = inner.glyphs.metrics.cell_size;
        let m = font.glyphs.metrics.cell_size;
        let nav = Self::nav_dev(inner) as f64;
        place.content.0 as f64 * m.width as f64 <= place.frame.cols as f64 * root.width as f64
            && place.content.1 as f64 * m.height as f64 + nav <= place.frame.rows as f64 * root.height as f64
    }

    /// A pane's cell in device px: its own font's when that is shown.
    fn pane_cell(inner: &Inner, place: &crate::layout::PanePlacement) -> (f64, f64) {
        let m = if Self::pane_font_shown(inner, place) {
            inner.pane_fonts[&place.pane_id].glyphs.metrics
        } else {
            inner.glyphs.metrics
        };
        (m.cell_size.width as f64, m.cell_size.height as f64)
    }

    /// The grid a placement shows at its own cell: the pane's own size,
    /// or less where its frame cannot hold it.
    fn shown_in(inner: &Inner, place: &crate::layout::PanePlacement) -> (usize, usize) {
        let (cw, ch) = Self::pane_cell(inner, place);
        let root = inner.glyphs.metrics.cell_size;
        let frame_w = place.frame.cols as f64 * root.width as f64;
        let frame_h = place.frame.rows as f64 * root.height as f64;
        let offset = Self::content_offset(inner, place) as f64;
        (
            place.content.0.min((frame_w / cw).floor() as usize),
            place.content.1.min(((frame_h - offset) / ch).floor().max(0.0) as usize),
        )
    }

    /// How far below its frame's top a pane's content starts: the bar.
    /// A pane the server has at its whole frame (a layout nobody with a
    /// bar has claimed yet) loses its last rows under the frame's bottom
    /// rather than its first under the bar -- the prompt is at the top,
    /// and the next claim from here makes room.
    fn content_offset(inner: &Inner, _place: &crate::layout::PanePlacement) -> f32 {
        Self::nav_dev(inner)
    }

    /// The panes as this page would claim them: the desktop's rule for
    /// each, at its own cell where it has its own font.
    fn native_panes(inner: &Inner, layout: &crate::layout::TabLayout) -> Vec<codec::ClientPaneViewport> {
        let nav_rows = Self::nav_rows(inner);
        let mut panes = layout.viewport(nav_rows);
        let root = inner.glyphs.metrics.cell_size;
        let nav = Self::nav_dev(inner) as f64;
        for pane in panes.iter_mut() {
            let Some(font) = inner.pane_fonts.get(&pane.pane_id) else {
                continue;
            };
            let Some(place) = layout.panes.iter().find(|p| p.pane_id == pane.pane_id) else {
                continue;
            };
            let (cw, ch) = (font.glyphs.metrics.cell_size.width as f64, font.glyphs.metrics.cell_size.height as f64);
            let cols = ((place.frame.cols as f64 * root.width as f64) / cw).floor().max(1.0) as usize;
            let rows = ((place.frame.rows as f64 * root.height as f64 - nav) / ch).floor().max(1.0) as usize;
            pane.size = TerminalSize {
                cols,
                rows,
                pixel_width: cols * cw as usize,
                pixel_height: rows * ch as usize,
                dpi: font.glyphs.dpi,
            };
        }
        panes
    }

    /// Cmd+= / Cmd+- scale the focused pane's font by a tenth, as the
    /// desktop does; Cmd+0 takes it back to the page's size. The pane's
    /// grid follows when this page holds the terminal.
    pub fn step_font(self: &Rc<Self>, by: f64) {
        let pane_id = self.inner.borrow().focused_pane;
        {
            let mut inner = self.inner.borrow_mut();
            let current = inner.pane_fonts.get(&pane_id).map(|f| f.scale).unwrap_or(1.0);
            let scale = if by == 0.0 { 1.0 } else { (current * 1.1f64.powf(by)).clamp(0.25, 4.0) };
            if (scale - 1.0).abs() < 1e-6 {
                inner.pane_fonts.remove(&pane_id);
            } else {
                let side = inner.glyphs.atlas.size() as u32;
                let (size_pt, dpi) = (inner.glyphs.size_pt, inner.glyphs.dpi);
                let built = GpuTexture::new(&inner.gpu.device, Arc::clone(&inner.gpu.queue), side, side).and_then(|t| {
                    GlyphCache::new(Rc::clone(&inner.fonts), size_pt * scale, dpi, Rc::new(t), Rc::clone(&inner.glyphs.families))
                });
                match built {
                    Ok(glyphs) => {
                        inner.pane_fonts.insert(
                            pane_id,
                            PaneFont { scale, glyphs, quads: HeapQuadAllocator::default(), vertices: Vec::new() },
                        );
                    }
                    Err(err) => {
                        Self::set_status(&inner, &failed("web-act-size-font", &err));
                        return;
                    }
                }
            }
            if let Some(layout) = &inner.tab_layout {
                let panes = Self::native_panes(&inner, layout);
                let mut lease = inner.link.lease_mut();
                lease.native = panes;
                lease.native_root = Some(layout.size);
            }
        }
        self.request_frame();
        // The pane's grid changes with its cell: claimed when this page
        // drives, and a page that does not takes the terminal first, as
        // any interaction here does.
        let (link, tab_id, owns) = {
            let inner = self.inner.borrow();
            let owns = inner.link.lease().owns_viewport();
            (inner.link.clone(), inner.tab_id, owns)
        };
        if owns {
            let app = Rc::clone(self);
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(err) = link.claim(tab_id).await {
                    log::warn!("resizing the pane for its font: {err:#}");
                }
                app.refresh_layout();
            });
        } else {
            self.take_over();
        }
    }

    /// The font size this page wants (`viewport::choose_size_pt`): the
    /// desktop's cell, and for a follower no larger than lets the whole
    /// tab fit the canvas. `None` when the size is right or pinned by
    /// `?font=`.
    fn desired_size_pt(inner: &Inner, dev_w: u32, dev_h: u32) -> Option<f64> {
        if inner.font_pinned {
            return None;
        }
        // Holding the tab, the page shows its own size; the desktop is
        // the one following now.
        if inner.link.lease().owns_viewport() {
            let base = inner.base_size_pt;
            return ((inner.glyphs.size_pt - base).abs() >= 0.125).then_some(base);
        }
        let (desktop, fit) = {
            let lease = inner.link.lease();
            (lease.canonical_size, lease.fit)
        };
        let desktop = desktop?;
        let page_dpi = 96.0 * inner.dpr;
        let dpi = page_dpi as u32;
        let fonts = Rc::clone(&inner.fonts);
        let cell = |pt: f64| {
            fonts.metrics(pt, dpi).ok().map(|m| {
                let r = crate::glyphs::RenderMetrics::with_font_metrics(&m);
                (r.cell_size.width as f64, r.cell_size.height as f64)
            })
        };
        let desktop_cell_h = (desktop.rows > 0 && desktop.dpi > 0 && desktop.pixel_height > 0)
            .then(|| desktop.pixel_height as f64 / desktop.rows as f64 / desktop.dpi as f64 * page_dpi);
        // Holding the terminal at this page's own shape, the tab takes the
        // grid this window has at the desktop's cell size; only a follower
        // shrinks the cell to show all of the desktop's tab.
        let grid = (!fit && desktop.cols > 0 && desktop.rows > 0).then_some((desktop.cols, desktop.rows));
        crate::viewport::choose_size_pt(cell, inner.glyphs.size_pt, desktop_cell_h, grid, (dev_w as f64, dev_h as f64))
    }

    /// The desktop's size changed: settle the font, and only then the
    /// grid. Nothing happens when the size is already right -- a forced
    /// pass through `resize` would re-claim the tab on every push.
    pub fn match_desktop_cell(&self) {
        let wanted = {
            let inner = self.inner.borrow();
            Self::desired_size_pt(&inner, inner.canvas.width(), inner.canvas.height())
        };
        if wanted.is_some() {
            self.inner.borrow_mut().cols = 0;
            self.resize();
            self.request_frame();
        }
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
            let size_pt = inner.glyphs.size_pt;
            Self::rerasterise(&mut inner, size_pt, (96.0 * dpr) as u32);
        }
        let dev_w = (rect.width() * dpr).floor().max(1.0) as u32;
        let dev_h = (rect.height() * dpr).floor().max(1.0) as u32;
        // Setting the backing store's size clears it: the frame is
        // painted again right away below, not at the next animation
        // frame, or the page's ground shows through for a frame.
        let cleared = inner.canvas.width() != dev_w || inner.canvas.height() != dev_h;
        if cleared {
            inner.canvas.set_width(dev_w);
            inner.canvas.set_height(dev_h);
        }
        inner.gpu.resize(dev_w, dev_h);
        // The font follows the desktop's cell and the window's box.
        if let Some(size_pt) = Self::desired_size_pt(&inner, dev_w, dev_h) {
            log::info!("font {} pt -> {size_pt} pt for the desktop's cell in this window", inner.glyphs.size_pt);
            let dpi = (96.0 * inner.dpr) as u32;
            Self::rerasterise(&mut inner, size_pt, dpi);
            inner.cols = 0;
        }
        let (cw, ch) = (
            inner.glyphs.metrics.cell_size.width as u32,
            inner.glyphs.metrics.cell_size.height as u32,
        );
        inner.mobile = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.body())
            .is_some_and(|body| body.has_attribute("data-mobile"));
        let pad = Self::pad(&inner);
        let Some((cols, rows)) = grid_for(
            dev_w.saturating_sub(2 * pad.0 as u32),
            dev_h.saturating_sub(Self::pad_y_total(&inner)),
            cw,
            ch,
        ) else {
            // A box too small for a cell is a hidden or collapsed page,
            // not a size anyone asked for: keep the last real grid rather
            // than resize every client's shell to it.
            return;
        };
        let changed = cols != inner.cols || rows != inner.rows;
        inner.cols = cols;
        inner.rows = rows;
        // The bars are placed from the cell size, which may just have moved.
        Self::notify(&inner);
        if changed {
            // The holder shapes the tab: a page that has the tab claims
            // its new grid, as a claim, because that is what changing the
            // tab's size is. A follower keeps the desktop's size and
            // letterboxes it.
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
                lease.owns_viewport() && !inner.panel_drag
            };
            if fitting {
                // Pane by pane, at frames scaled to the new grid: one
                // claim that reserves every bar's rows, rather than a bare
                // grid the listing then corrects (two reflows on screen).
                if let Some(scaled) = Self::scaled_layout(&inner, cols, rows) {
                    let panes = Self::native_panes(&inner, &scaled);
                    let sizes: Vec<(PaneId, TerminalSize)> = panes.iter().map(|p| (p.pane_id, p.size)).collect();
                    let mut lease = inner.link.lease_mut();
                    lease.native = panes;
                    lease.native_root = Some(size);
                    drop(lease);
                    Self::preview_panes(&mut inner, &sizes);
                }
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
        if cleared {
            self.frame();
        } else {
            self.request_frame();
        }
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
        // Output in a parked tab keeps its cell current but changes
        // nothing on screen: no paint for it alone.
        let dirty = inner.host.events.take_dirty();
        if !dirty.is_empty() && dirty.iter().all(|pane| !inner.panes.contains_key(pane)) {
            return;
        }
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
            // The extra row a fractional scroll draws is one whose fetch can
            // stall like any other; asking about the range that was painted
            // is what keeps a dropped `GetLines` for it being retried.
            let visible =
                visible_rows_px(&dims, Self::shown(&place).1, cell.scroll_from_bottom, cell.scroll_px);
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
        Self::rebuild_pane_fonts(inner);
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
        Self::rebuild_glyphs(inner, texture)?;
        Self::rebuild_pane_fonts(inner);
        Ok(())
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
            stack: vec![],
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
        let frozen = inner.capacity.frozen();
        inner.glyphs.begin_frame(frozen);
        for font in inner.pane_fonts.values_mut() {
            font.glyphs.begin_frame(frozen);
            font.quads.recycle();
        }
        let (w, h) = inner.gpu.size();
        let surface = (w as f32, h as f32);
        let root_w = inner.glyphs.metrics.cell_size.width as f32;
        let root_h = inner.glyphs.metrics.cell_size.height as f32;
        let owns = inner.link.lease().owns_viewport();
        let placements = Self::placements(inner);
        let focused_palette = inner.focused().palette.clone();
        let pad = Self::pad(inner);
        let tab = inner.tab_layout.as_ref().map(|l| (l.cols, l.rows)).unwrap_or((usize::MAX, usize::MAX));
        inner.quads.recycle();
        let smooth = inner.settings.scroll_mode.is_smooth();
        for place in &placements {
            let (cols_shown, rows_shown) = Self::shown_in(inner, place);
            let offset = Self::content_offset(inner, place);
            // The pane's own cell height, before its glyph cache is borrowed
            // to draw with: what a fractional scroll offset is measured in.
            let pane_cell_h = Self::pane_cell(inner, place).1 as f32;
            let Some(cell) = inner.panes.get_mut(&place.pane_id) else {
                continue;
            };
            let session = Arc::clone(&cell.session);
            let dims = session.dimensions();
            let max = max_scroll(&dims);
            if cell.scroll_from_bottom > max {
                cell.scroll_from_bottom = max;
                cell.scroll_px = 0.0;
            }
            // The offset only means anything inside the row it is measured
            // against, and only above the tail: a font change, a switch to
            // stepped mode or a scroll back to the newest row all leave it
            // behind, and a whole row is what each of those wants.
            if !smooth
                || cell.scroll_from_bottom == 0
                || !(cell.scroll_px > 0.0)
                || cell.scroll_px >= pane_cell_h
            {
                cell.scroll_px = 0.0;
            }
            let px = cell.scroll_px;
            let visible = visible_rows_px(&dims, rows_shown, cell.scroll_from_bottom, px);
            let (first, lines) = session.get_lines(visible);
            // No cursor before the server has placed one: a fresh cell's
            // sits at the origin, which is nowhere real.
            let mut cursor = session.cursor_position();
            if !session.has_received() {
                cursor.y = StableRowIndex::MIN;
            }
            let palette = cell.palette.clone();
            let selection = cell.selection;
            let is_focused = place.pane_id == inner.focused_pane;
            let hsv = if is_focused { None } else { Some(INACTIVE_PANE_HSB) };
            // A pane with its own font draws from its own cache, into its
            // own batch.
            let own_font = Self::pane_font_shown(inner, place);
            let (glyphs, quads): (&mut GlyphCache, &mut HeapQuadAllocator) = match inner.pane_fonts.get_mut(&place.pane_id) {
                Some(font) if own_font => (&mut font.glyphs, &mut font.quads),
                _ => (&mut inner.glyphs, &mut inner.quads),
            };
            let scratch = &mut inner.scratch;
            let cell_w = glyphs.metrics.cell_size.width as f32;
            let cell_h = glyphs.metrics.cell_size.height as f32;
            let frame_origin = (
                pad.0 + place.frame.left as f32 * root_w,
                pad.1 + place.frame.top as f32 * root_h,
            );
            // The content sits below the pane's bar, as on the desktop, and
            // a fractional scroll lifts the rows by the part of the top one
            // that is cut off; the extra row fetched above fills the strip
            // that leaves at the bottom.
            let content_top = frame_origin.1 + offset;
            let origin = (frame_origin.0, content_top - px);
            let clip = (cols_shown as f32 * cell_w, rows_shown as f32 * cell_h);
            // The content box, in the quads' own centre-relative space: what
            // the two rows hanging over its edges are cropped to.
            let content_clip = thinkterm_render::quad::QuadClipRect::from_top_left_pixels(
                frame_origin.0,
                content_top,
                frame_origin.0 + clip.0,
                content_top + clip.1,
                &thinkterm_render::geom::Dimensions {
                    pixel_width: w as usize,
                    pixel_height: h as usize,
                    dpi: dims.dpi as usize,
                },
            );
            // The pane's own ground: its frame, plus the padding out to the
            // canvas edge where it touches the tab's edge and half a cell
            // into the divider where it does not, so the panes tile the
            // canvas and an inactive pane's dimming shows no rim.
            let (tab_cols, tab_rows) = (tab.0, tab.1);
            let x0 = if place.frame.left == 0 { 0.0 } else { frame_origin.0 - root_w / 2.0 };
            let y0 = if place.frame.top == 0 { 0.0 } else { frame_origin.1 - root_h / 2.0 };
            let x1 = if place.frame.left + place.frame.cols >= tab_cols {
                surface.0
            } else {
                frame_origin.0 + place.frame.cols as f32 * root_w + root_w / 2.0
            };
            let y1 = if place.frame.top + place.frame.rows >= tab_rows {
                surface.1
            } else {
                frame_origin.1 + place.frame.rows as f32 * root_h + root_h / 2.0
            };
            crate::emit::fill_rect(
                glyphs,
                quads,
                0,
                surface,
                (x0, y0),
                (x1 - x0, y1 - y0),
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
            let last = lines.len().saturating_sub(1);
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
                if px > 0.0 && (i == 0 || i == last) {
                    // Only the two rows that hang over the content box are
                    // worth cropping quad by quad: the clip does a bilerp
                    // and a box per quad, which no interior row needs. The
                    // scratch buffer is the one kept on `Inner`, emptied
                    // rather than allocated.
                    scratch.recycle();
                    crate::emit::emit_line(glyphs, scratch, budget, &params)?;
                    scratch.apply_to_clipped_at(&mut *quads, 0.0, 0.0, content_clip, 1.0)?;
                } else {
                    crate::emit::emit_line(glyphs, quads, budget, &params)?;
                }
            }
        }
        let (cell_w, cell_h) = (root_w, root_h);
        // Dividers, centred in the gap cell like the desktop's.
        if let Some(layout) = &inner.tab_layout {
            let t = (inner.glyphs.metrics.underline_height.max(1)) as f32;
            let colour = focused_palette.split.to_linear();
            // Centred in the gap cell, and as long as the desktop's: half a
            // cell into the neighbouring gaps, to the canvas edge where the
            // gap reaches the tab's edge (`paint_split`).
            for divider in &layout.dividers {
                let (x0, y0, x1, y1) = match *divider {
                    crate::layout::Divider::Col { col, top, rows } => {
                        let x = pad.0 + col as f32 * cell_w + (cell_w - t) / 2.0;
                        let y0 = if top == 0 { 0.0 } else { pad.1 + top as f32 * cell_h - cell_h / 2.0 };
                        let y1 = if top + rows >= layout.rows { surface.1 } else { pad.1 + (top + rows) as f32 * cell_h + cell_h / 2.0 };
                        (x, y0, x + t, y1)
                    }
                    crate::layout::Divider::Row { row, left, cols } => {
                        let y = pad.1 + row as f32 * cell_h + (cell_h - t) / 2.0;
                        let x0 = if left == 0 { 0.0 } else { pad.0 + left as f32 * cell_w - cell_w / 2.0 };
                        let x1 = if left + cols >= layout.cols { surface.0 } else { pad.0 + (left + cols) as f32 * cell_w + cell_w / 2.0 };
                        (x0, y, x1, y + t)
                    }
                };
                crate::emit::fill_rect(&inner.glyphs, &mut inner.quads, 0, surface, (x0, y0), (x1 - x0, y1 - y0), colour, None)?;
            }
            // A follower larger than the tab sees the desktop's faint
            // grid over what the tab does not reach.
            let lease = inner.link.lease();
            let follower = matches!(lease.mode, Some(codec::FrontendAccessMode::TmuxLatest)) && !lease.owns_viewport();
            drop(lease);
            if follower && (inner.cols > layout.cols || inner.rows > layout.rows) {
                let faint = wezterm_color_types::LinearRgba::with_srgba(0x8e, 0x8e, 0x93, 26);
                let (tab_w, tab_h) = (layout.cols as f32 * cell_w, layout.rows as f32 * cell_h);
                let (page_w, page_h) = (inner.cols as f32 * cell_w, inner.rows as f32 * cell_h);
                let mut line = |x: f32, y: f32, w: f32, h: f32| {
                    crate::emit::fill_rect(&inner.glyphs, &mut inner.quads, 0, surface, (pad.0 + x, pad.1 + y), (w, h), faint, None)
                };
                for c in layout.cols..=inner.cols {
                    line(c as f32 * cell_w, 0.0, 1.0, page_h)?;
                }
                for r in layout.rows..=inner.rows {
                    line(0.0, r as f32 * cell_h, page_w, 1.0)?;
                }
                for r in 0..layout.rows {
                    line(tab_w, r as f32 * cell_h, page_w - tab_w, 1.0)?;
                }
                for c in 0..layout.cols {
                    line(c as f32 * cell_w, tab_h, 1.0, page_h - tab_h)?;
                }
            }
        }
        inner.vertices.clear();
        inner.quads.extract_vertices(&mut inner.vertices);
        let mut declined = inner.glyphs.declined();
        for font in inner.pane_fonts.values_mut() {
            font.vertices.clear();
            font.quads.extract_vertices(&mut font.vertices);
            declined += font.glyphs.declined();
        }
        // The cleared ground, for anyone reading the canvas from the page:
        // the pixels themselves are behind WebGPU.
        let hex = focused_palette.background.to_rgb_string();
        if inner.published_bg.as_deref() != Some(hex.as_str()) {
            let _ = inner.canvas.set_attribute("data-bg", &hex);
            inner.published_bg = Some(hex);
        }
        let bg = focused_palette.background.to_linear().tuple();
        let millis = (js_sys::Date::now() % (u32::MAX as f64)) as u32;
        let mut batches: Vec<(&[Vertex], &GpuTexture)> = vec![(&inner.vertices, inner.glyphs.texture())];
        for font in inner.pane_fonts.values() {
            batches.push((&font.vertices, font.glyphs.texture()));
        }
        inner.gpu.draw_batches(&batches, [bg.0, bg.1, bg.2, bg.3], millis)?;
        Ok((budget.deferred(), declined))
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
