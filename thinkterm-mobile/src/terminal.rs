//! One pane, end to end: the handshake that attaches to the server's active
//! pane, the session that keeps its lines, and the frame that turns those
//! lines into quads through the shared emitter and glyph atlas.
//!
//! This is the S0.5 slice: the parts of the browser's App that one pane
//! needs, and nothing of the rest (layout, splits, the tree, menus).

use crate::host::MobileHost;
use crate::link::SshLink;
use anyhow::{anyhow, bail, Result};
use codec::Pdu;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use thinkterm_proto::{ClientId, PaneId, RenderableDimensions, TabId, WindowId};
use thinkterm_render::pipeline::GpuTexture;
use thinkterm_render::quad::HeapQuadAllocator;
use thinkterm_render::vertex::Vertex;
use thinkterm_session::host::{request, Spawner};
use thinkterm_session::input::InputQueueFull;
use thinkterm_session::pane::PaneSession;
use thinkterm_web::chrome::active_pane;
use thinkterm_web::emit;
use thinkterm_web::fallback::FallbackBudget;
use thinkterm_web::glyphs::GlyphCache;
use thinkterm_web::keymap::{map_key, DomKey};
use thinkterm_web::viewport::{max_scroll, visible_rows};
use wezterm_color_types::LinearRgba;
use wezterm_term::color::ColorPalette;
use wezterm_term::{KeyModifiers, StableRowIndex, TerminalSize};

pub struct Attached {
    pub pane_id: PaneId,
    pub tab_id: TabId,
    pub window_id: WindowId,
    pub workspace: String,
    pub title: String,
    pub dims: RenderableDimensions,
    pub alt_screen: bool,
    pub server_version: String,
    pub server_id: String,
}

/// The browser's `attach()`, minus the nav rows and the native (per-pane)
/// viewport: a phone reports one cell grid.
pub async fn attach(link: &SshLink, size: Option<TerminalSize>, wall_ms: u64) -> Result<Attached> {
    let version = request(
        link,
        Pdu::GetCodecVersion(codec::GetCodecVersion {}),
        |pdu| match pdu {
            Pdu::GetCodecVersionResponse(v) => Ok(v),
            other => Err(other),
        },
    )
    .await?;
    if version.codec_vers != codec::CODEC_VERSION {
        bail!(
            "this app speaks protocol {} but the server speaks {}; update the server or the app",
            codec::CODEC_VERSION,
            version.codec_vers
        );
    }

    let me = ClientId {
        hostname: "ios".into(),
        username: "mobile".into(),
        pid: std::process::id(),
        epoch: wall_ms,
        id: (wall_ms as usize) ^ (std::process::id() as usize) << 8,
        ssh_auth_sock: None,
    };
    link.lease_mut().me = Some(me.clone());
    request(
        link,
        Pdu::SetClientId(codec::SetClientId {
            client_id: me,
            is_proxy: false,
        }),
        |pdu| match pdu {
            Pdu::UnitResponse(_) => Ok(()),
            other => Err(other),
        },
    )
    .await?;

    let panes = request(link, Pdu::ListPanes(codec::ListPanes {}), |pdu| match pdu {
        Pdu::ListPanesResponse(p) => Ok(p),
        other => Err(other),
    })
    .await?;
    let entry = panes
        .tabs
        .iter()
        .find_map(active_pane)
        .ok_or_else(|| anyhow!("the server has no panes to show"))?;
    {
        let mut lease = link.lease_mut();
        lease.tab_id = Some(entry.tab_id);
        lease.reported = size;
        lease.canonical_size = Some(entry.size);
    }
    link.report_viewport(entry.tab_id).await?;

    request(
        link,
        Pdu::GetPaneRenderChanges(codec::GetPaneRenderChanges {
            pane_id: entry.pane_id,
        }),
        |pdu| match pdu {
            Pdu::LivenessResponse(_) | Pdu::UnitResponse(_) => Ok(()),
            other => Err(other),
        },
    )
    .await?;

    let rows = entry.size.rows;
    Ok(Attached {
        pane_id: entry.pane_id,
        tab_id: entry.tab_id,
        window_id: entry.window_id,
        workspace: entry.workspace.clone(),
        title: entry.title.clone(),
        dims: RenderableDimensions {
            cols: entry.size.cols,
            viewport_rows: rows,
            scrollback_rows: rows,
            physical_top: entry.physical_top,
            scrollback_top: entry.physical_top,
            dpi: entry.size.dpi,
            pixel_width: entry.size.pixel_width,
            pixel_height: entry.size.pixel_height,
            reverse_video: false,
        },
        alt_screen: entry.alt_screen,
        server_version: version.version_string,
        server_id: version.server_id,
    })
}

/// Nothing to come back to: the tab and the pane are both gone.
#[derive(Debug)]
pub struct NoPanes;

impl std::fmt::Display for NoPanes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the server has no panes to show")
    }
}

impl std::error::Error for NoPanes {}

/// The browser's `reattach()`: the same identity on a new transport, the
/// tab found again (or the pane's tab, or any tab), the viewport reported
/// and the delta stream restarted.
pub async fn reattach(
    link: &SshLink,
    tab_id: TabId,
    pane_id: PaneId,
    size: Option<TerminalSize>,
) -> Result<codec::ListPanesResponse> {
    let version = request(
        link,
        Pdu::GetCodecVersion(codec::GetCodecVersion {}),
        |pdu| match pdu {
            Pdu::GetCodecVersionResponse(v) => Ok(v),
            other => Err(other),
        },
    )
    .await?;
    if version.codec_vers != codec::CODEC_VERSION {
        bail!(
            "this app speaks protocol {} but the server speaks {}; update the server or the app",
            codec::CODEC_VERSION,
            version.codec_vers
        );
    }
    let me = link
        .lease()
        .me
        .clone()
        .ok_or_else(|| anyhow!("reconnecting without an identity"))?;
    request(
        link,
        Pdu::SetClientId(codec::SetClientId {
            client_id: me,
            is_proxy: false,
        }),
        |pdu| match pdu {
            Pdu::UnitResponse(_) => Ok(()),
            other => Err(other),
        },
    )
    .await?;
    let panes = request(link, Pdu::ListPanes(codec::ListPanes {}), |pdu| match pdu {
        Pdu::ListPanesResponse(p) => Ok(p),
        other => Err(other),
    })
    .await?;
    let tab = panes
        .tabs
        .iter()
        .find(|tab| tab.window_and_tab_ids().is_some_and(|(_, id)| id == tab_id))
        .or_else(|| panes.tabs.iter().find(|tab| contains_pane(tab, pane_id)))
        .ok_or(NoPanes)?;
    let (_, tab_id) = tab.window_and_tab_ids().ok_or(NoPanes)?;
    if !contains_pane(tab, pane_id) {
        return Err(NoPanes.into());
    }
    {
        let mut lease = link.lease_mut();
        lease.tab_id = Some(tab_id);
        lease.tab_owner = None;
        lease.reported = size;
        lease.reported_viewport = None;
        lease.fit = false;
    }
    link.report_viewport(tab_id).await?;
    request(
        link,
        Pdu::GetPaneRenderChanges(codec::GetPaneRenderChanges { pane_id }),
        |pdu| match pdu {
            Pdu::LivenessResponse(_) | Pdu::UnitResponse(_) => Ok(()),
            other => Err(other),
        },
    )
    .await?;
    Ok(panes)
}

fn contains_pane(node: &thinkterm_proto::layout::PaneNode, pane_id: PaneId) -> bool {
    use thinkterm_proto::layout::PaneNode;
    match node {
        PaneNode::Empty => false,
        PaneNode::Leaf(entry) => entry.pane_id == pane_id,
        PaneNode::Stack(stack) => stack.panes.iter().any(|e| e.pane_id == pane_id),
        PaneNode::Split { left, right, .. } => {
            contains_pane(left, pane_id) || contains_pane(right, pane_id)
        }
    }
}

/// The grid a surface of `w`x`h` device pixels holds, or `None` when it
/// cannot hold even one cell.
pub fn grid_for(w: u32, h: u32, cell_w: u32, cell_h: u32) -> Option<(usize, usize)> {
    if cell_w == 0 || cell_h == 0 || w < cell_w || h < cell_h {
        return None;
    }
    Some(((w / cell_w).max(2) as usize, (h / cell_h).max(1) as usize))
}

pub struct Terminal {
    pub glyphs: GlyphCache,
    quads: HeapQuadAllocator,
    vertices: Vec<Vertex>,
    pub session: Arc<PaneSession<MobileHost>>,
    host: Arc<MobileHost>,
    pub pane_id: PaneId,
    pub tab_id: TabId,
    pub palette: ColorPalette,
    pub cols: usize,
    pub rows: usize,
    pub dpi: u32,
    scroll_from_bottom: usize,
    /// The IME owns the keyboard while this is set: keys are dropped and
    /// only committed text gets through.
    pub composing: bool,
    /// Inputs handed to the session, for the probe to count.
    pub inputs_sent: u64,
}

impl Terminal {
    pub fn new(
        host: Arc<MobileHost>,
        glyphs: GlyphCache,
        attached: &Attached,
        cols: usize,
        rows: usize,
        dpi: u32,
    ) -> Self {
        let images = Arc::new(thinkterm_session::Lock::new(
            thinkterm_session::images::ImageStore::default(),
        ));
        let remote_tab_id = Arc::new(AtomicUsize::new(attached.tab_id));
        let session = PaneSession::new(
            Arc::clone(&host),
            images,
            thinkterm_session::SessionConfig {
                scrollback_lines: 3500,
                local_echo_threshold_ms: Some(100),
                overlay_lag_indicator: false,
            },
            attached.pane_id,
            remote_tab_id,
            attached.pane_id,
            attached.dims,
            &attached.title,
            attached.alt_screen,
        );
        Self {
            glyphs,
            quads: HeapQuadAllocator::default(),
            vertices: Vec::new(),
            session,
            host,
            pane_id: attached.pane_id,
            tab_id: attached.tab_id,
            palette: ColorPalette::default(),
            cols,
            rows,
            dpi,
            scroll_from_bottom: 0,
            composing: false,
            inputs_sent: 0,
        }
    }

    pub fn cell_size(&self) -> (u32, u32) {
        (
            self.glyphs.metrics.cell_size.width as u32,
            self.glyphs.metrics.cell_size.height as u32,
        )
    }

    pub fn size(&self) -> TerminalSize {
        let (cw, ch) = self.cell_size();
        TerminalSize {
            rows: self.rows,
            cols: self.cols,
            pixel_width: self.cols * cw as usize,
            pixel_height: self.rows * ch as usize,
            dpi: self.dpi,
        }
    }

    /// The surface changed size: fit the grid to it and tell the server.
    /// Returns whether the grid changed.
    pub fn resize(&mut self, cols: usize, rows: usize) -> bool {
        if (cols, rows) == (self.cols, self.rows) {
            return false;
        }
        self.cols = cols;
        self.rows = rows;
        let size = self.size();
        self.session.apply_local_resize(size);
        self.host.link.lease_mut().reported = Some(size);
        let link = self.host.link.clone();
        let tab_id = self.tab_id;
        self.host.spawner.spawn_detached(Box::pin(async move {
            if let Err(err) = link.claim(tab_id).await {
                log::warn!("fitting the tab to this screen: {err:#}");
            }
        }));
        true
    }

    fn spawn_drain(&self, start: Result<bool, InputQueueFull>) {
        match start {
            Ok(true) => {
                let session = Arc::clone(&self.session);
                self.host
                    .spawner
                    .spawn_detached(Box::pin(session.drain_inputs()));
            }
            Ok(false) => {}
            Err(err) => log::warn!("input refused: {err:?}"),
        }
    }

    /// A key by its DOM name ("Enter", "ArrowUp", "a"...), as the key bar
    /// and a hardware keyboard send them.
    pub fn key(&mut self, name: &str, ctrl: bool, alt: bool, shift: bool) -> bool {
        let dom = DomKey {
            key: name,
            code: "",
            ctrl,
            alt,
            shift,
            meta: false,
            composing: false,
        };
        if self.composing {
            return false;
        }
        let Some((key, mods)) = map_key(&dom) else {
            return false;
        };
        let serial = codec::InputSerial::from_millis(thinkterm_session::clock::Clock::wall_millis(
            &self.host.clock,
        ));
        let mods = KeyModifiers::from_bits_truncate(mods.bits());
        self.scroll_from_bottom = 0;
        self.inputs_sent += 1;
        let start = self.session.key_down(serial, key, mods);
        self.spawn_drain(start);
        true
    }

    /// Committed text (the soft keyboard, an IME's final choice).
    pub fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.scroll_from_bottom = 0;
        self.inputs_sent += 1;
        let start = self.session.write_bytes(text.as_bytes());
        self.spawn_drain(start);
    }

    pub fn paste(&mut self, text: &str) {
        self.scroll_from_bottom = 0;
        self.inputs_sent += 1;
        let start = self.session.paste(text);
        self.spawn_drain(start);
    }

    /// The transport is back: every line is suspect until fetched again.
    pub fn reconnected(&mut self) {
        self.session.set_dead(false);
        self.session.make_all_stale();
    }

    pub fn scroll_lines(&mut self, delta: i32) {
        let max = max_scroll(&self.session.dimensions());
        let next = self.scroll_from_bottom as i64 + delta as i64;
        self.scroll_from_bottom = next.clamp(0, max as i64) as usize;
    }

    /// Everything the server pushed that concerns this pane.
    pub fn on_push(&mut self, pdu: Pdu) -> bool {
        match pdu {
            Pdu::GetPaneRenderChangesResponse(delta) if delta.pane_id == self.pane_id => {
                self.session.queue_render_delta(delta);
                false
            }
            Pdu::PaneRemoved(removed) if removed.pane_id == self.pane_id => {
                self.session.set_dead(true);
                true
            }
            Pdu::DefaultPalette(codec::DefaultPalette { palette }) => {
                self.palette = palette;
                true
            }
            Pdu::SetApplicationPalette(codec::SetApplicationPalette { pane_id, palette })
                if pane_id == self.pane_id =>
            {
                if let Some(palette) = palette {
                    self.palette = palette;
                }
                true
            }
            Pdu::ClientViewportState(_) | Pdu::FrontendAccessState(_) => true,
            _ => false,
        }
    }

    /// Build this frame's quads. Returns the vertices, the atlas they
    /// index, and the background to clear with.
    pub fn paint(
        &mut self,
        surface: (u32, u32),
        budget: &mut FallbackBudget,
    ) -> Result<(&[Vertex], &GpuTexture, LinearRgba)> {
        self.glyphs.begin_frame(false);
        self.quads.recycle();
        let surface_f = (surface.0 as f32, surface.1 as f32);
        let cell_w = self.glyphs.metrics.cell_size.width as f32;
        let cell_h = self.glyphs.metrics.cell_size.height as f32;

        let dims = self.session.dimensions();
        let max = max_scroll(&dims);
        if self.scroll_from_bottom > max {
            self.scroll_from_bottom = max;
        }
        let visible = visible_rows(&dims, self.rows, self.scroll_from_bottom);
        let (first, lines) = self.session.get_lines(visible);
        let mut cursor = self.session.cursor_position();
        if !self.session.has_received() {
            cursor.y = StableRowIndex::MIN;
        }
        let palette = self.palette.clone();
        let background = palette.background.to_linear();

        emit::fill_rect(
            &self.glyphs,
            &mut self.quads,
            0,
            surface_f,
            (0.0, 0.0),
            surface_f,
            background,
            None,
        )?;
        let origin = (0.0, 0.0);
        let clip = (self.cols as f32 * cell_w, self.rows as f32 * cell_h);
        for (i, line) in lines.iter().enumerate() {
            let row = first + i as StableRowIndex;
            let params = emit::LineParams {
                line,
                stable_row: row,
                top_pixel_y: i as f32 * cell_h,
                cursor: &cursor,
                palette: &palette,
                selection: 0..0,
                focused: true,
                reverse_video: dims.reverse_video,
                surface: surface_f,
                origin,
                clip,
                hsv: None,
                draw_cursor: true,
            };
            emit::emit_line(&mut self.glyphs, &mut self.quads, budget, &params)?;
        }
        self.vertices.clear();
        self.quads.extract_vertices(&mut self.vertices);
        Ok((&self.vertices, self.glyphs.texture(), background))
    }
}
