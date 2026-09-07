//! The page's state: one pane session, the canvas it is drawn on, where
//! the user is looking, what they selected, and who owns the tab.

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
    pub pane_id: PaneId,
    pub tab_id: TabId,
    pub dpr: f64,
    pub cols: usize,
    pub rows: usize,
    pub title: String,
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
    disconnected: Option<String>,
    quads: HeapQuadAllocator,
    vertices: Vec<Vertex>,
    title: String,
}

pub struct App {
    inner: RefCell<Inner>,
    frame_requested: Cell<bool>,
    raf: RefCell<Option<Closure<dyn FnMut()>>>,
}

fn now_ms() -> f64 {
    js_sys::Date::now()
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
            disconnected: None,
            quads: HeapQuadAllocator::default(),
            vertices: Vec::new(),
            title: setup.title,
        };
        let app = Rc::new(Self {
            inner: RefCell::new(inner),
            frame_requested: Cell::new(false),
            raf: RefCell::new(None),
        });
        let weak = Rc::downgrade(&app);
        *app.raf.borrow_mut() = Some(Closure::<dyn FnMut()>::new(move || {
            if let Some(app) = weak.upgrade() {
                app.frame_requested.set(false);
                app.frame();
            }
        }));
        app
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
    pub fn on_push(&self, pdu: Pdu) {
        let inner = self.inner.borrow();
        match pdu {
            Pdu::GetPaneRenderChangesResponse(delta) if delta.pane_id == inner.pane_id => {
                inner.session.queue_render_delta(delta);
            }
            Pdu::PaneRemoved(removed) if removed.pane_id == inner.pane_id => {
                inner.session.set_dead(true);
                Self::set_status(&inner, "the pane was closed on the server");
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

    pub fn on_close(&self, reason: String) {
        let mut inner = self.inner.borrow_mut();
        inner.disconnected = Some(reason.clone());
        // Nothing more will arrive: the watchdog must stop asking, or the
        // page keeps requesting frames for lines that never come.
        inner.session.set_dead(true);
        Self::set_status(&inner, &format!("disconnected: {reason}. Reload to reconnect."));
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

    /// Fit the grid to the canvas's CSS box at the device pixel ratio, and
    /// tell the server if this browser owns the viewport.
    pub fn resize(&self) {
        let mut inner = self.inner.borrow_mut();
        let rect = inner.canvas.get_bounding_client_rect();
        let dpr = web_sys::window().map(|w| w.device_pixel_ratio()).unwrap_or(1.0);
        if (dpr - inner.dpr).abs() > f64::EPSILON {
            // Another monitor: glyphs are rasterised for the new density.
            inner.dpr = dpr;
            let dpi = (96.0 * dpr) as u32;
            let side = inner.glyphs.atlas.size() as u32;
            match GpuTexture::new(&inner.gpu.device, Arc::clone(&inner.gpu.queue), side, side)
                .and_then(|t| GlyphCache::new(Rc::clone(&inner.fonts), inner.glyphs.size_pt, dpi, Rc::new(t)))
            {
                Ok(glyphs) => inner.glyphs = glyphs,
                Err(err) => log::error!("glyph cache for dpr {dpr}: {err:#}"),
            }
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
                        log::warn!("resize refused: {err:#}");
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
        let _ = inner.host.events.take_dirty();
        let title = inner.session.title();
        if title != inner.title {
            inner.title = title;
            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                doc.set_title(&format!("{} — ThinkTerm", inner.title));
            }
            drop(inner);
            self.refresh_status();
            inner = self.inner.borrow_mut();
        }
        for attempt in 0..2 {
            match Self::paint(&mut inner) {
                Ok(()) => break,
                Err(err) if err.root_cause().downcast_ref::<OutOfTextureSpace>().is_some() && attempt == 0 => {
                    if let Err(err) = Self::grow_atlas(&mut inner) {
                        log::error!("atlas could not grow: {err:#}");
                        break;
                    }
                }
                Err(err) => {
                    log::error!("frame failed: {err:#}");
                    break;
                }
            }
        }
        // Lines still in flight, or a stalled fetch: come back for them.
        let dims = inner.session.dimensions();
        let visible = visible_rows(&dims, inner.rows, inner.scroll_from_bottom);
        // Exactly the rows shown: asking about rows past a shorter page's
        // bottom, which may not exist, would repaint forever.
        if inner.session.render_looks_stalled_in(visible) {
            drop(inner);
            self.request_frame();
        }
    }

    fn grow_atlas(inner: &mut Inner) -> Result<()> {
        let side = (inner.glyphs.atlas.size() * 2).min(inner.gpu.max_texture_dimension() as usize);
        if side <= inner.glyphs.atlas.size() {
            anyhow::bail!("the atlas is already at the GPU's largest texture");
        }
        let texture = Rc::new(GpuTexture::new(&inner.gpu.device, Arc::clone(&inner.gpu.queue), side as u32, side as u32)?);
        inner.glyphs = GlyphCache::new(Rc::clone(&inner.fonts), inner.glyphs.size_pt, inner.glyphs.dpi, texture)?;
        Ok(())
    }

    fn paint(inner: &mut Inner) -> Result<()> {
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
            };
            crate::emit::emit_line(&mut inner.glyphs, &mut inner.quads, &params)?;
        }
        inner.vertices.clear();
        inner.quads.extract_vertices(&mut inner.vertices);
        let bg = inner.palette.background.to_linear().tuple();
        let millis = (js_sys::Date::now() % (u32::MAX as f64)) as u32;
        inner
            .gpu
            .draw(&inner.vertices, inner.glyphs.texture(), [bg.0, bg.1, bg.2, bg.3], millis)
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
