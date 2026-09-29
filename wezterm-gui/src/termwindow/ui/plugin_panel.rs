//! A plugin's panel in the right sidebar: the view the plugin host serves
//! this window, the player that shows it, and how the player's items are
//! painted with the sidebar's own quads, fonts and colours.
//!
//! Every item is recorded into a heap first and laid on the frame cut to
//! its region, which is how a row half out of a list comes out half drawn.
//! Lines and filled areas become columns a pixel wide, only where they
//! show: the quads the renderer draws are upright rectangles, and cutting a
//! column is cutting a rectangle. What was recorded is kept, and laid again
//! frame after frame while the panel stays as it was: a terminal busy
//! beside it repaints the window, not the panel.
//!
//! The panel is opened when it comes on show and closed when it goes: a
//! plugin runs, and keeps its data, only while someone looks at it.
//!
//! A panel whose frames ask for it has an extended view as well, left of
//! the sidebar, as wide as the user drags it (`right_sidebar`): a second
//! view the host serves this window, with a player of its own, opened on
//! each opening of the panel once the panel is drawn on it, and closed
//! when the panel stops asking.
//!
//! Beside a terminal on another machine reached over SSH, a panel is told
//! which, and its plugin asks this window to run programs and read files
//! there: over the connection Files reaches that machine with, and only
//! once the user has let ThinkTerm connect to it -- in Files, or with the
//! button the panel shows in place of what the plugin drew until then.

use crate::plugins::{self, PanelNews};
use crate::quad::{HeapQuadAllocator, QuadClipRect, QuadTrait, TripleLayerQuadAllocator};
use crate::termwindow::remote_files::{
    authorize_remote_source, remote_connection_key, remote_connection_manager,
    remote_source_is_authorized, run_command_line, RemoteAcquireError, RemoteFileKind,
    RemoteFilesState, RemotePath,
};
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER_OUTLINE, BOTTOM_RIGHT_ROUNDED_CORNER_OUTLINE,
    TOP_LEFT_ROUNDED_CORNER_OUTLINE, TOP_RIGHT_ROUNDED_CORNER_OUTLINE,
};
use crate::termwindow::{TermWindow, TermWindowNotif, UIItem, UIItemType};
use crate::ui::UiPalette;
use crate::utilsprites::RenderMetrics;
use crate::workspace_threads;
use anyhow::Context;
use mux::domain::{Domain, DomainId};
use mux::pane::{CachePolicy, Pane, PaneId};
use mux::Mux;
use std::collections::hash_map::RandomState;
use std::convert::TryFrom;
use std::hash::BuildHasher;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use thinkterm_plugin_channel::wire::{PanelRequest, Raw};
use thinkterm_plugin_panel::{
    Align, Answer, Area, Ask, Bounds, Button, Bytes, CloseButton, Color, Cursor, Draw, Drawn,
    Entry, EntryKind, Env, Font, Input, Line, Mods, MonoMetrics, Player, Rect, Remote, Size, Text,
    TextMetrics, Token,
};
use url::Url;
use wezterm_font::LoadedFont;
use window::color::LinearRgba;
use window::{MouseCursor, MouseEvent, MouseEventKind as WMEK, MousePress, RectF, WindowOps};

/// How long a panel the host closed, but may serve again, waits before it
/// is opened anew: a restart is under way.
const REOPEN_AFTER: Duration = Duration::from_millis(400);
/// A plugin's panel draws everything on the text layer, in its own order:
/// a rectangle drawn after text covers it.
const LAYER: usize = 2;
/// The least time between two sizes told to a plugin: a window being
/// resized changes its panel's size each frame, and each is drawn anew.
const ENV_EVERY: Duration = Duration::from_millis(100);
/// The most quads a painting is recorded with: a glyph each, a column for
/// each pixel a line or an area crosses. What is beyond it is left out.
const QUAD_LIMIT: usize = 100_000;
/// The most a plugin's ask brings back from another machine: with the
/// answer carried as base64, it fits in a frame to the plugin.
const ASK_BYTES: usize = 8 * 1024 * 1024;
/// How long a program a plugin runs on another machine may take.
const ASK_RUN_WAIT: Duration = Duration::from_secs(20);
/// How long how the machine beside the panel is reached is kept before it
/// is found again.
const REACH_AGAIN: Duration = Duration::from_secs(3);

pub(crate) struct PluginPanel {
    plugin: String,
    /// The panel in the sidebar.
    shown: Shown,
    /// Counts the panel's openings: its extended view is opened on one, and
    /// again on the next.
    opening: u32,
    /// Its extended view, and the opening of the panel it was opened on.
    extended: Option<(Shown, u32)>,
    /// How wide the user made the extended view, in window pixels; 0 for
    /// the width it starts at.
    pub(crate) extended_width: usize,
    /// The user closed the extended view with its close button: it is not
    /// opened again until the panel's frames have stopped asking for it.
    dismissed: bool,
}

/// A view the host serves this window -- a panel, or its extended view --
/// and the player that shows it.
struct Shown {
    view: u64,
    player: Player,
    status: Status,
    /// Where it was last painted, in window pixels, and pixels to a unit:
    /// for turning the pointer into its units.
    origin: (f32, f32),
    scale: f32,
    /// What was last recorded, and what it was recorded for.
    painted: Option<Painted>,
    /// When the plugin was last told its size, and whether the player has
    /// one since that it is still to be told: rows asked for meanwhile,
    /// which are drawn for it, wait until it is.
    env_told: Instant,
    env_due: bool,
    /// Its plugin asked something of the machine beside it, which ThinkTerm
    /// may not connect to yet: shown instead of what it drew.
    connect: Option<Connect>,
}

/// A machine a plugin needs ThinkTerm connected to, and the source Files
/// knows it by, which the user lets ThinkTerm connect to.
struct Connect {
    host: String,
    /// The machine, as the view's env names it ([`Reach::machine`]).
    machine: String,
    source_key: String,
}

/// Where the terminal beside the panel is, as far as is known.
enum Beside {
    /// On this machine, in this directory.
    Here(String),
    /// On another, in a directory there.
    There(Remote),
    /// Not known yet: the panel stays where it was until it is.
    Unknown,
    /// Where no plugin can follow: a terminal whose directory names nothing
    /// a plugin can reach, or on a machine that cannot be reached.
    Nowhere,
}

/// How the machine beside the panel is reached: the connection Files keeps
/// to it.
#[derive(Clone)]
pub(crate) struct Reach {
    /// The machine, by the name the user is shown.
    host: String,
    /// The connection, by the word its plugin is told ([`machine_word`]).
    machine: String,
    source_key: String,
    connection_key: String,
    config: config::SshDomain,
}

impl Shown {
    fn new(view: u64, player: Player) -> Self {
        Self {
            view,
            player,
            status: Status::Opening,
            origin: (0.0, 0.0),
            scale: 1.0,
            painted: None,
            env_told: Instant::now(),
            env_due: false,
            connect: None,
        }
    }

    /// Whether the pointer at `x`, `y` in window pixels is over it, and
    /// where in its units.
    fn under(&self, x: f32, y: f32) -> Option<(f32, f32)> {
        let x = (x - self.origin.0) / self.scale;
        let y = (y - self.origin.1) / self.scale;
        let env = self.player.env();
        (x >= 0.0 && y >= 0.0 && x < env.width && y < env.height).then_some((x, y))
    }
}

impl PluginPanel {
    pub(crate) fn plugin(&self) -> &str {
        &self.plugin
    }

    /// Whether the panel's frames ask for its extended view, and it is to
    /// have one: not once it cannot be served.
    pub(crate) fn wants_extended(&self) -> bool {
        self.shown.player.shown()
            && self.shown.player.extend()
            && !self.dismissed
            && !matches!(self.shown.status, Status::Stopped(_))
    }
}

/// A painting recorded as quads, a heap for each region it is cut to.
struct Painted {
    key: PaintKey,
    regions: Vec<(Bounds, HeapQuadAllocator)>,
}

/// Everything a recorded painting depends on: the same key paints the same.
#[derive(PartialEq)]
struct PaintKey {
    revision: u64,
    origin: (f32, f32),
    scale: f32,
    window: (usize, usize),
    /// Bumped when the glyphs the quads point into may have moved.
    shapes: usize,
    chrome: UiPalette,
    sizes: [f64; 3],
    mono_size: f64,
}

enum Status {
    /// Opened, and nothing drawn yet.
    Opening,
    Open,
    /// The host closed it, and it is opened again at this time.
    Again(Instant),
    /// The host cannot serve it, for this reason.
    Stopped(String),
    /// The host cannot be reached, for this reason: it is opened again once
    /// it can, and keeps its room meanwhile -- an extended view's too.
    Lost(String),
}

/// What a panel and its extended view are painted with in one frame: the
/// fonts, pixels to a unit, and where the terminal beside them is -- its
/// directory here, or the other machine it runs on.
pub(crate) struct PanelPaint {
    fonts: PanelFonts,
    scale: f32,
    cwd: Option<String>,
    remote: Option<Remote>,
}

/// The fonts a panel's text is set in, for one paint.
struct PanelFonts {
    small: (Rc<LoadedFont>, RenderMetrics),
    body: (Rc<LoadedFont>, RenderMetrics),
    title: (Rc<LoadedFont>, RenderMetrics),
    small_bold: (Rc<LoadedFont>, RenderMetrics),
    body_bold: (Rc<LoadedFont>, RenderMetrics),
    title_bold: (Rc<LoadedFont>, RenderMetrics),
    mono: (Rc<LoadedFont>, RenderMetrics),
    sizes: [f64; 3],
    mono_size: f64,
}

impl PanelFonts {
    fn of(&self, text: &Text) -> &(Rc<LoadedFont>, RenderMetrics) {
        match (text.font, text.size, text.bold) {
            (Font::Mono, ..) => &self.mono,
            (Font::Ui, Size::Small, false) => &self.small,
            (Font::Ui, Size::Body, false) => &self.body,
            (Font::Ui, Size::Title, false) => &self.title,
            (Font::Ui, Size::Small, true) => &self.small_bold,
            (Font::Ui, Size::Body, true) => &self.body_bold,
            (Font::Ui, Size::Title, true) => &self.title_bold,
        }
    }
}

/// Physical pixels to one of a panel's units: a point, as a browser's CSS
/// pixel is, so a panel is laid out alike on the desktop and on a page.
pub(crate) fn panel_scale(dpi: usize) -> f32 {
    let points = if cfg!(target_os = "macos") {
        72.0
    } else {
        96.0
    };
    (dpi.max(1) as f32 / points).max(0.25)
}

/// The colours a panel names, in the chrome's palette. The palette has no
/// green or amber, so those are fixed, a shade apart for dark and light.
/// The grounds are solid: laid on the panel's own ground beforehand, since
/// a see-through green blends in linear light and reads twice as strong.
pub(crate) fn token_color(chrome: &UiPalette, token: Token) -> LinearRgba {
    let dark = chrome.is_dark();
    let pick = |dark_rgb: (u8, u8, u8), light_rgb: (u8, u8, u8)| {
        let (r, g, b) = if dark { dark_rgb } else { light_rgb };
        LinearRgba::with_srgba(r, g, b, 255)
    };
    let positive = pick((63, 185, 80), (26, 127, 55));
    let negative = pick((248, 81, 73), (207, 34, 46));
    let ground = chrome.workspace_sidebar_bg;
    let (faint, strong) = if dark { (0.16, 0.34) } else { (0.12, 0.26) };
    match token {
        Token::Text => chrome.text,
        Token::TextMuted => chrome.secondary_text,
        Token::TextFaint => chrome.muted_text,
        Token::Bg => chrome.workspace_sidebar_bg,
        Token::BgRaised => chrome.sidebar_button_bg,
        Token::BgHover => chrome.sidebar_row_hover_bg,
        Token::BgSelected => chrome.sidebar_row_active_bg,
        Token::Border => chrome.separator,
        Token::Accent => chrome.accent,
        Token::OnAccent => chrome.on_accent,
        Token::Positive => positive,
        Token::Negative => negative,
        Token::Warning => pick((210, 153, 34), (154, 103, 0)),
        Token::PositiveBg => mixed(ground, positive, faint),
        Token::NegativeBg => mixed(ground, negative, faint),
        Token::PositiveBgStrong => mixed(ground, positive, strong),
        Token::NegativeBgStrong => mixed(ground, negative, strong),
    }
}

/// `over` laid on `ground` at `amount`, mixed as sRGB is.
fn mixed(ground: LinearRgba, over: LinearRgba, amount: f32) -> LinearRgba {
    let (r0, g0, b0, _) = ground.srgba_pixel().as_rgba();
    let (r1, g1, b1, _) = over.srgba_pixel().as_rgba();
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * amount).round() as u8;
    LinearRgba::with_srgba(mix(r0, r1), mix(g0, g1), mix(b0, b1), 255)
}

fn color(chrome: &UiPalette, color: Color) -> LinearRgba {
    match color {
        Color::Token(token) => token_color(chrome, token),
        Color::Rgba([r, g, b, a]) => LinearRgba::with_srgba(r, g, b, a),
    }
}

impl TermWindow {
    /// What a panel and its extended view are painted with in a frame,
    /// worked out once for both.
    pub(crate) fn plugin_panel_paint(&mut self) -> anyhow::Result<PanelPaint> {
        let (cwd, remote) = match self.plugin_panel_beside() {
            Beside::Here(cwd) => (Some(cwd), None),
            Beside::There(remote) => (None, Some(remote)),
            Beside::Nowhere => (None, None),
            // Where it was, for now: told nothing new, the plugin keeps
            // what it shows.
            Beside::Unknown => match &self.right_sidebar_plugin {
                Some(panel) => {
                    let env = panel.shown.player.env();
                    (env.cwd.clone(), env.remote.clone())
                }
                None => (None, None),
            },
        };
        Ok(PanelPaint {
            fonts: self.plugin_panel_fonts()?,
            scale: panel_scale(self.dimensions.dpi),
            remote,
            cwd,
        })
    }

    /// Paints plugin `plugin`'s panel in the content area, opening it first
    /// if this window does not show it yet.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_plugin_panel(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        plugin: &str,
        paint: &PanelPaint,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
    ) -> anyhow::Result<()> {
        let height = content_bottom.saturating_sub(content_top);
        if content_width == 0 || height == 0 {
            return Ok(());
        }
        let PanelPaint {
            fonts,
            scale,
            cwd,
            remote,
        } = paint;
        let scale = *scale;
        let mut env = panel_env(fonts, &chrome, content_width as f32, height as f32, scale);
        env.cwd = cwd.clone();
        env.remote = remote.clone();
        env.can_extend = self.right_sidebar_plugin_can_extend();
        self.follow_plugin_panel(plugin, env);
        // The extended view goes once nothing asks for it, or there is no
        // room left for it.
        if self.right_sidebar_plugin_extended_rect().is_none() {
            self.close_plugin_extended();
        }
        self.ui_items.push(UIItem {
            x: content_x,
            y: content_top,
            width: content_width,
            height,
            item_type: UIItemType::RightSidebarPluginPanel,
        });
        let Some(mut panel) = self.right_sidebar_plugin.take() else {
            return Ok(());
        };
        let area = (content_x, content_top, content_width);
        let painted = self.paint_plugin_view(
            layers,
            &mut panel.shown,
            false,
            area,
            scale,
            ui_font,
            ui_metrics,
            fonts,
            &chrome,
        );
        self.right_sidebar_plugin = Some(panel);
        painted
    }

    /// Paints the extended view of the panel on show in its area, opening
    /// it first when it is not open on the panel's opening. `close` is the
    /// close button the caller draws over it, in window pixels -- left,
    /// top, side -- which its plugin is told of.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_plugin_panel_extended(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        paint: &PanelPaint,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
        close: (usize, usize, usize),
    ) -> anyhow::Result<()> {
        let height = content_bottom.saturating_sub(content_top);
        if content_width == 0 || height == 0 {
            return Ok(());
        }
        let PanelPaint {
            fonts,
            scale,
            cwd,
            remote,
        } = paint;
        let scale = *scale;
        let mut env = panel_env(fonts, &chrome, content_width as f32, height as f32, scale);
        env.cwd = cwd.clone();
        env.remote = remote.clone();
        let (x, y, size) = close;
        env.close = Some(CloseButton {
            x: (x as f32 - content_x as f32) / scale,
            y: (y as f32 - content_top as f32) / scale,
            size: size as f32 / scale,
        });
        self.follow_plugin_extended(env);
        self.ui_items.push(UIItem {
            x: content_x,
            y: content_top,
            width: content_width,
            height,
            item_type: UIItemType::RightSidebarPluginExtended,
        });
        let Some(mut panel) = self.right_sidebar_plugin.take() else {
            return Ok(());
        };
        let area = (content_x, content_top, content_width);
        let painted = match panel.extended.as_mut() {
            Some((extended, _)) => self.paint_plugin_view(
                layers, extended, true, area, scale, ui_font, ui_metrics, fonts, &chrome,
            ),
            None => Ok(()),
        };
        self.right_sidebar_plugin = Some(panel);
        painted
    }

    /// Paints one view from the top left corner of `area`, `width` wide:
    /// what its player draws, or what it says instead while it has nothing
    /// to show -- or the button that lets ThinkTerm connect to the machine
    /// beside it, while its plugin waits for that.
    #[allow(clippy::too_many_arguments)]
    fn paint_plugin_view(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        shown: &mut Shown,
        extended: bool,
        area: (usize, usize, usize),
        scale: f32,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        fonts: &PanelFonts,
        chrome: &UiPalette,
    ) -> anyhow::Result<()> {
        let (x, y, width) = area;
        shown.origin = (x as f32, y as f32);
        shown.scale = scale;
        // What the pointer is over is settled here as well as on each move:
        // leaving the view for another part of the window sends it no event.
        let pointer = self
            .current_mouse_event
            .as_ref()
            .and_then(|event| shown.under(event.coords.x as f32, event.coords.y as f32));
        match pointer {
            Some((x, y)) => {
                shown.player.pointer_moved(x, y);
            }
            None => {
                shown.player.pointer_left();
            }
        }

        // Wanted no longer once the terminal beside it is elsewhere, or the
        // user let ThinkTerm connect, in Files say.
        let wanted = shown.connect.as_ref().is_some_and(|connect| {
            let beside = shown.player.env().remote.as_ref();
            beside.is_some_and(|remote| remote.machine == connect.machine)
                && !remote_source_is_authorized(&connect.source_key)
        });
        if !wanted {
            shown.connect = None;
        }
        if let Some(connect) = &shown.connect {
            let inset = (8.0 * scale) as usize;
            let lead = shown
                .player
                .env()
                .close
                .map_or(0, |close| ((close.x + close.size) * scale) as usize);
            let left = x + lead + inset;
            let room = width.saturating_sub(lead + inset * 2);
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("host", connect.host.clone());
            let needs = crate::i18n::tr_args("right-plugin-connect", &args);
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                &needs,
                left,
                y + inset,
                room,
                chrome.secondary_text,
            )?;
            let button_top = y + inset * 2 + ui_metrics.cell_size.height as usize;
            let button_width = room.min(self.ui_px(160));
            return self.paint_remote_files_connect_button(
                layers,
                ui_font,
                ui_metrics,
                *chrome,
                left,
                button_top,
                button_width,
                &crate::i18n::tr("right-connect"),
                true,
                UIItemType::RightSidebarPluginConnect(extended),
            );
        }

        let message = match &shown.status {
            Status::Opening | Status::Again(_) if !shown.player.shown() => {
                Some(crate::i18n::tr("right-plugin-starting"))
            }
            Status::Stopped(reason) | Status::Lost(reason) => {
                let mut args = fluent_bundle::FluentArgs::new();
                args.set("reason", reason.clone());
                Some(crate::i18n::tr_args("right-plugin-stopped", &args))
            }
            _ => None,
        };
        if let Some(message) = message {
            let inset = (8.0 * scale) as usize;
            // Right of an extended view's close button.
            let lead = shown
                .player
                .env()
                .close
                .map_or(0, |close| ((close.x + close.size) * scale) as usize);
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                &message,
                x + lead + inset,
                y + inset,
                width.saturating_sub(lead + inset * 2),
                chrome.secondary_text,
            )?;
            return Ok(());
        }
        self.paint_plugin_panel_items(layers, shown, fonts, chrome)
    }

    /// Opens plugin `plugin`'s panel for `env` unless this window shows it
    /// already, closing any other; tells the plugin a size or theme that
    /// changed, the last of those within [`ENV_EVERY`]; and opens a panel
    /// the host closed again once it is time.
    fn follow_plugin_panel(&mut self, plugin: &str, env: Env) {
        if self
            .right_sidebar_plugin
            .as_ref()
            .is_some_and(|panel| panel.plugin != plugin)
        {
            self.close_plugin_panel();
        }
        let Some(panel) = self.right_sidebar_plugin.as_mut() else {
            let Some(window) = self.window.clone() else {
                return;
            };
            let view = plugins::open_panel(plugin, &env, None, &window);
            let extended_width = super::right_sidebar::right_sidebar_plugin_extended_width_for(
                plugin,
                self.dimensions.dpi,
            );
            self.right_sidebar_plugin = Some(PluginPanel {
                plugin: plugin.to_string(),
                shown: Shown::new(view, Player::new(env)),
                opening: 0,
                extended: None,
                extended_width,
                dismissed: false,
            });
            return;
        };
        let now = Instant::now();
        let shown = &mut panel.shown;
        if let Status::Again(at) = shown.status {
            if now >= at {
                shown.player.set_env(env);
                shown.player.restarted();
                shown.status = Status::Opening;
                shown.env_told = now;
                shown.env_due = false;
                panel.opening = panel.opening.wrapping_add(1);
                plugins::reopen_panel(shown.view, &panel.plugin, shown.player.env());
            } else {
                self.update_next_frame_time(Some(at));
            }
            return;
        }
        if let Some(due) = tell_env(shown, env, now) {
            self.update_next_frame_time(Some(due));
        }
    }

    /// Opens the extended view of the panel on show for `env` unless it is
    /// open on the panel's opening -- once the panel is drawn on that, one
    /// opened on an earlier showing what it last had meanwhile -- and tells
    /// the plugin a size or theme that changed, as for the panel.
    fn follow_plugin_extended(&mut self, env: Env) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(panel) = self.right_sidebar_plugin.as_mut() else {
            return;
        };
        let opening = panel.opening;
        match panel.extended.as_mut() {
            Some((extended, on)) if *on == opening => {
                if let Some(due) = tell_env(extended, env, Instant::now()) {
                    self.update_next_frame_time(Some(due));
                }
            }
            _ if !matches!(panel.shown.status, Status::Open) => {}
            _ => {
                let player = match panel.extended.take() {
                    Some((stale, _)) => {
                        plugins::close_panel(stale.view);
                        let mut player = stale.player;
                        player.set_env(env);
                        player.restarted();
                        player
                    }
                    None => Player::new(env),
                };
                let view = plugins::open_panel(
                    &panel.plugin,
                    player.env(),
                    Some(panel.shown.view),
                    &window,
                );
                panel.extended = Some((Shown::new(view, player), opening));
            }
        }
    }

    /// Opens the right sidebar on plugin `plugin`'s panel, if the sidebar
    /// offers it.
    pub(crate) fn show_plugin_panel(&mut self, plugin: &str) {
        let Some(id) = crate::termwindow::PanelId::new(plugin) else {
            log::warn!("no plugin is named {plugin:?}");
            return;
        };
        let mode = crate::termwindow::RightSidebarMode::Plugin(id);
        if !mode.panel_enabled() {
            log::warn!("the right sidebar offers no panel of plugin {plugin}");
            return;
        }
        self.right_sidebar_mode = mode;
        if self.right_sidebar_collapsed {
            self.expand_right_sidebar();
        }
        if let Some(window) = self.window.as_ref().cloned() {
            let dimensions = self.dimensions;
            self.apply_dimensions(&dimensions, None, &window);
            window.invalidate();
        }
    }

    /// Lets go of the panel on show, if one is, and of its extended view:
    /// the plugin is told, and what they held is freed.
    pub(crate) fn close_plugin_panel(&mut self) {
        if let Some(panel) = self.right_sidebar_plugin.take() {
            if let Some((extended, _)) = panel.extended {
                plugins::close_panel(extended.view);
            }
            plugins::close_panel(panel.shown.view);
        }
        self.plugin_panel_domains.clear();
        self.plugin_panel_reach = None;
    }

    /// The extended view's close button pressed: the view goes at once,
    /// its plugin is told the user closed it, and the panel gets none until
    /// its frames have stopped asking for one.
    pub(crate) fn dismiss_plugin_extended(&mut self) {
        let before = self.right_sidebar_width();
        let Some(panel) = self.right_sidebar_plugin.as_mut() else {
            return;
        };
        panel.dismissed = true;
        if let Some((extended, _)) = panel.extended.take() {
            let input = Raw::new(&Input::Close);
            plugins::tell_panel(extended.view, PanelRequest::Input { input });
            plugins::close_panel(extended.view);
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
        self.reflow_right_sidebar_if_width_changed(before);
    }

    /// Lets go of the extended view of the panel on show, if it has one.
    fn close_plugin_extended(&mut self) {
        let extended = self
            .right_sidebar_plugin
            .as_mut()
            .and_then(|panel| panel.extended.take());
        if let Some((extended, _)) = extended {
            plugins::close_panel(extended.view);
        }
    }

    /// Where the terminal of the pane in focus in this window is, for the
    /// plugin panel: its directory here, once a look off this thread said
    /// it runs on this machine, where the plugin does -- the path of one on
    /// a host reached over SSH names nothing here -- or the other machine
    /// and its directory there. Not known yet while the look is on its way,
    /// or the terminal has not said where it is -- a tab just opened, whose
    /// shell has only begun -- or while the window has no pane in focus, as
    /// on the way to a new tab's.
    fn plugin_panel_beside(&mut self) -> Beside {
        let Some(pane) = self.get_active_pane_no_overlay() else {
            return Beside::Unknown;
        };
        match self.plugin_pane_runs_here(pane.domain_id()) {
            None => Beside::Unknown,
            Some(true) => match pane.get_current_working_dir(CachePolicy::AllowStale) {
                None => Beside::Unknown,
                Some(url) => local_path(&url).map_or(Beside::Nowhere, Beside::Here),
            },
            Some(false) => self.plugin_panel_remote(pane.pane_id(), &pane),
        }
    }

    /// Whether the panes of domain `domain_id` run on this machine: `None`
    /// until a look off this thread says, once for the domain. Another tab
    /// of a domain the panel was beside already is known at once, so the
    /// panel's directory does not go missing while the answer comes, and
    /// its plugin does not let go of what it shows there.
    fn plugin_pane_runs_here(&mut self, domain_id: DomainId) -> Option<bool> {
        if let Some((_, here)) = self
            .plugin_panel_domains
            .iter()
            .find(|(id, _)| *id == domain_id)
        {
            return *here;
        }
        match Mux::get().get_domain(domain_id) {
            Some(domain) => {
                self.plugin_panel_domains.push((domain_id, None));
                self.look_where_domain_runs(domain_id, domain);
                None
            }
            None => {
                self.plugin_panel_domains.push((domain_id, Some(false)));
                Some(false)
            }
        }
    }

    /// The other machine pane `pane_id`, `pane`, runs on, reached over SSH,
    /// and its directory there, for the plugin panel: once a look off this
    /// thread said the pane does not run here. The machine is found the way
    /// Files finds it, once for each pane.
    fn plugin_panel_remote(&mut self, pane_id: PaneId, pane: &Arc<dyn Pane>) -> Beside {
        let (host, machine) = match self.plugin_panel_reach_of(pane_id) {
            Ok(reach) => (reach.host.clone(), reach.machine.clone()),
            Err(_) => return Beside::Nowhere,
        };
        match pane.get_current_working_dir(CachePolicy::AllowStale) {
            None => Beside::Unknown,
            Some(url) => remote_path(&url).map_or(Beside::Nowhere, |cwd| {
                Beside::There(Remote { host, machine, cwd })
            }),
        }
    }

    /// How the machine the pane in focus runs on is reached.
    fn plugin_panel_reach(&mut self) -> Result<Reach, String> {
        let pane_id = self
            .get_active_pane_no_overlay()
            .ok_or_else(|| "no terminal is beside the panel".to_string())?
            .pane_id();
        self.plugin_panel_reach_of(pane_id).clone()
    }

    /// How the machine pane `pane_id` runs on is reached: found for the
    /// pane when it comes beside the panel, as Files finds it, and again
    /// off this thread once it is [`REACH_AGAIN`] old, so that a host
    /// edited in the host list, or a domain not yet there when it was
    /// first looked for, is followed. Each frame takes only its names.
    fn plugin_panel_reach_of(&mut self, pane_id: PaneId) -> &Result<Reach, String> {
        match &self.plugin_panel_reach {
            Some((id, found, _)) if *id == pane_id => {
                if found.elapsed() >= REACH_AGAIN {
                    self.find_plugin_panel_reach_again(pane_id);
                }
            }
            _ => {
                let reach = self.plugin_remote_reach();
                self.plugin_panel_reach = Some((pane_id, Instant::now(), reach));
            }
        }
        match &self.plugin_panel_reach {
            Some((_, _, reach)) => reach,
            None => unreachable!("just found"),
        }
    }

    /// Finds how the machine pane `pane_id` runs on is reached again, on a
    /// thread of its own: the connection's settings are read from disk,
    /// which a frame is not to wait for. Kept while the pane is still the
    /// one beside the panel, which is painted again if its machine is now
    /// another.
    fn find_plugin_panel_reach_again(&mut self, pane_id: PaneId) {
        let target = match self.active_remote_project_for_files() {
            Ok(Some(target)) => target,
            found => {
                let why = found.err().unwrap_or_else(|| {
                    "the terminal beside the panel runs on this machine".to_string()
                });
                self.plugin_panel_reach = Some((pane_id, Instant::now(), Err(why)));
                return;
            }
        };
        // Not looked for again until this look is as old.
        if let Some((_, found, _)) = &mut self.plugin_panel_reach {
            *found = Instant::now();
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        promise::spawn::spawn(async move {
            let reach = promise::spawn::spawn_into_new_thread(move || Ok(Self::reach_of(&target)))
                .await
                .unwrap_or_else(|err| Err(format!("{err:#}")));
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let Some((id, found, before)) = &mut term_window.plugin_panel_reach else {
                    return;
                };
                if *id != pane_id {
                    return;
                }
                let moved = match (&*before, &reach) {
                    (Ok(before), Ok(now)) => {
                        (&before.host, &before.machine) != (&now.host, &now.machine)
                    }
                    (Err(_), Err(_)) => false,
                    _ => true,
                };
                *found = Instant::now();
                *before = reach;
                if moved {
                    if let Some(window) = term_window.window.as_ref() {
                        window.invalidate();
                    }
                }
            })));
        })
        .detach();
    }

    /// How the machine the pane in focus runs on is reached: the project's,
    /// as Files reaches it.
    fn plugin_remote_reach(&self) -> Result<Reach, String> {
        let target = self
            .active_remote_project_for_files()?
            .ok_or_else(|| "the terminal beside the panel runs on this machine".to_string())?;
        Self::reach_of(&target)
    }

    /// How `target`, a project's machine, is reached. Reads the connection's
    /// settings from disk -- the host list, ~/.ssh/config, the key a saved
    /// password is kept under -- so any thread may call it.
    fn reach_of(target: &workspace_threads::RemoteFilesTarget) -> Result<Reach, String> {
        let config = Self::ssh_config_for_remote_files_target(target)?;
        let source_key = RemoteFilesState::source_key(&target.source);
        let connection_key = remote_connection_key(&source_key, &config);
        Ok(Reach {
            host: host_name(&config.remote_address),
            machine: machine_word(&connection_key),
            source_key,
            connection_key,
            config,
        })
    }

    /// Does what the plugin of view `view` asked of the machine beside its
    /// panel, over the connection Files reaches it with, and answers. One
    /// the user has not let ThinkTerm open is not opened for a plugin: the
    /// view shows a button that lets it instead, and the plugin is told to
    /// ask again after.
    fn plugin_panel_ask(&mut self, view: u64, id: u64, machine: String, ask: Ask) {
        let reach = match self.plugin_panel_reach() {
            Ok(reach) => reach,
            Err(why) => return plugins::answer(view, id, &Answer::failed(why)),
        };
        // Asked for the terminal's machine before it moved to another: not
        // done where it is now.
        if reach.machine != machine {
            let why = "the terminal beside the panel is on another machine now";
            return plugins::answer(view, id, &Answer::failed(why));
        }
        if !remote_source_is_authorized(&reach.source_key) {
            let why = format!("ThinkTerm is not connected to {}", reach.host);
            let connect = Connect {
                host: reach.host,
                machine: reach.machine,
                source_key: reach.source_key,
            };
            if let Some(shown) = self.plugin_view_numbered(view) {
                shown.connect = Some(connect);
            }
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
            return plugins::answer(view, id, &Answer::Failed { why, connect: true });
        }
        promise::spawn::spawn(async move {
            let answer = ask_there(reach, ask).await;
            plugins::answer(view, id, &answer);
        })
        .detach();
    }

    /// The button a view shows while its plugin waits for a connection,
    /// pressed: ThinkTerm may connect to that machine from now on, as when
    /// Files was let, and the plugin's next ask does.
    pub(crate) fn plugin_panel_connect(&mut self, extended: bool) {
        let connect = self
            .plugin_view_mut(extended)
            .and_then(|shown| shown.connect.take());
        if let Some(connect) = connect {
            authorize_remote_source(&connect.source_key);
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// The view of this window's panel numbered `view`: the panel, or its
    /// extended view.
    fn plugin_view_numbered(&mut self, view: u64) -> Option<&mut Shown> {
        let panel = self.right_sidebar_plugin.as_mut()?;
        if panel.shown.view == view {
            return Some(&mut panel.shown);
        }
        panel
            .extended
            .as_mut()
            .map(|(extended, _)| extended)
            .filter(|extended| extended.view == view)
    }

    /// Finds out, on a thread of its own, whether the panes of `domain`,
    /// `domain_id`, run on this machine, and paints the panel again once it
    /// knows: on Windows, telling a WSL domain from a plain one asks WSL.
    fn look_where_domain_runs(&self, domain_id: DomainId, domain: Arc<dyn Domain>) {
        let Some(window) = self.window.clone() else {
            return;
        };
        promise::spawn::spawn(async move {
            let here = promise::spawn::spawn_into_new_thread(move || Ok(runs_here(&domain)))
                .await
                .unwrap_or(false);
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                // Gone with the panel meanwhile, and asked again with the next.
                let looked = term_window
                    .plugin_panel_domains
                    .iter_mut()
                    .find(|(id, _)| *id == domain_id);
                if let Some((_, known)) = looked {
                    *known = Some(here);
                    if let Some(window) = term_window.window.as_ref() {
                        window.invalidate();
                    }
                }
            })));
        })
        .detach();
    }

    /// What the host said about the view `view`: the panel on show, or its
    /// extended view. The terminal is laid out again when the sidebar's
    /// width changed with it.
    pub(crate) fn plugin_panel_heard(&mut self, view: u64, news: PanelNews) {
        let news = match news {
            PanelNews::Remote { id, machine, ask } => {
                if self.plugin_view_numbered(view).is_none() {
                    // A view this window let go of meanwhile.
                    return plugins::answer(view, id, &Answer::failed("the panel closed"));
                }
                return self.plugin_panel_ask(view, id, machine, ask);
            }
            news => news,
        };
        let before = self.right_sidebar_width();
        let Some(panel) = self.right_sidebar_plugin.as_mut() else {
            return;
        };
        if panel.shown.view == view {
            let shown = &mut panel.shown;
            match news {
                PanelNews::Reconnected => {
                    shown.player.restarted();
                    shown.status = Status::Opening;
                    shown.env_told = Instant::now();
                    shown.env_due = false;
                    panel.opening = panel.opening.wrapping_add(1);
                    plugins::reopen_panel(view, &panel.plugin, shown.player.env());
                }
                news => heard(shown, &panel.plugin, news),
            }
            // Once its frames stop asking, the next that asks opens it.
            if panel.shown.player.shown() && !panel.shown.player.extend() {
                panel.dismissed = false;
            }
        } else if let Some((extended, _)) = panel
            .extended
            .as_mut()
            .filter(|(extended, _)| extended.view == view)
        {
            // One the host no longer serves stays as it is until the panel's
            // next opening, when it is opened anew.
            heard(extended, &panel.plugin, news);
        } else {
            // A view this window let go of meanwhile.
            return;
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
        self.reflow_right_sidebar_if_width_changed(before);
    }

    /// The pointer over the panel, or its extended view: a hover tints at
    /// once, a press goes to the plugin.
    pub(crate) fn mouse_event_plugin_panel(
        &mut self,
        event: MouseEvent,
        context: &dyn WindowOps,
        extended: bool,
    ) {
        let Some(shown) = self.plugin_view_mut(extended) else {
            context.set_cursor(Some(MouseCursor::Arrow));
            return;
        };
        let x = (event.coords.x as f32 - shown.origin.0) / shown.scale;
        let y = (event.coords.y as f32 - shown.origin.1) / shown.scale;
        let mut repaint = shown.player.pointer_moved(x, y);
        let button = match event.kind {
            WMEK::Press(MousePress::Left) => Some(Button::Left),
            WMEK::Press(MousePress::Middle) => Some(Button::Middle),
            WMEK::Press(MousePress::Right) => Some(Button::Right),
            _ => None,
        };
        if let Some(button) = button {
            let count = match &self.last_mouse_click {
                Some(click) if click.streak > 1 && button == Button::Left => click.streak as u32,
                _ => 1,
            };
            let mods = Mods {
                shift: event.modifiers.contains(::window::Modifiers::SHIFT),
                ctrl: event.modifiers.contains(::window::Modifiers::CTRL),
                alt: event.modifiers.contains(::window::Modifiers::ALT),
                cmd: event.modifiers.contains(::window::Modifiers::SUPER),
            };
            let shown = self.plugin_view_mut(extended).expect("found above");
            if let Some(input) = shown.player.click(x, y, button, count, mods) {
                let input = Raw::new(&input);
                plugins::tell_panel(shown.view, PanelRequest::Input { input });
                repaint = true;
            }
        }
        let cursor = self
            .plugin_view_mut(extended)
            .and_then(|shown| shown.player.cursor());
        context.set_cursor(Some(match cursor {
            Some(Cursor::Pointer) => MouseCursor::Hand,
            Some(Cursor::Arrow) | None => MouseCursor::Arrow,
        }));
        if repaint {
            context.invalidate();
        }
    }

    /// The wheel turned `dx` pixels across and `dy` down over the panel or
    /// its extended view, whichever the pointer is over. True when
    /// something in it scrolled.
    pub(crate) fn plugin_panel_wheel(&mut self, event: &MouseEvent, dx: f32, dy: f32) -> bool {
        let Some(panel) = self.right_sidebar_plugin.as_mut() else {
            return false;
        };
        let (px, py) = (event.coords.x as f32, event.coords.y as f32);
        let extended = panel
            .extended
            .as_mut()
            .map(|(extended, _)| extended)
            .filter(|extended| extended.under(px, py).is_some());
        let shown = extended.unwrap_or(&mut panel.shown);
        let x = (px - shown.origin.0) / shown.scale;
        let y = (py - shown.origin.1) / shown.scale;
        let moved = shown.player.wheel(x, y, dx / shown.scale, dy / shown.scale);
        if moved {
            send_wanted(shown);
        }
        moved
    }

    /// The pointer left the panel, or its extended view.
    pub(crate) fn plugin_panel_pointer_left(&mut self, extended: bool) -> bool {
        self.plugin_view_mut(extended)
            .is_some_and(|shown| shown.player.pointer_left())
    }

    /// The panel on show, or its extended view.
    fn plugin_view_mut(&mut self, extended: bool) -> Option<&mut Shown> {
        let panel = self.right_sidebar_plugin.as_mut()?;
        if extended {
            panel.extended.as_mut().map(|(extended, _)| extended)
        } else {
            Some(&mut panel.shown)
        }
    }

    fn plugin_panel_fonts(&self) -> anyhow::Result<PanelFonts> {
        let settings = crate::native_settings::load_shared();
        let body = crate::native_settings::right_sidebar_font_size(&settings);
        let sizes = [(body - 1.5).max(8.0), body, body + 2.0];
        let with = |font: Rc<LoadedFont>| {
            let metrics = RenderMetrics::with_font_metrics(&font.metrics());
            (font, metrics)
        };
        let plain =
            |size: f64| -> anyhow::Result<_> { Ok(with(self.fonts.title_font_with_size(size)?)) };
        let bold = |size: f64| -> anyhow::Result<_> {
            Ok(with(self.fonts.title_font_with_size_and_weight(size, 700)?))
        };
        let mono = self
            .fonts
            .resolve_font(&self.config.font)
            .context("plugin panel monospaced font")?;
        Ok(PanelFonts {
            small: plain(sizes[0])?,
            body: plain(sizes[1])?,
            title: plain(sizes[2])?,
            small_bold: bold(sizes[0])?,
            body_bold: bold(sizes[1])?,
            title_bold: bold(sizes[2])?,
            mono: with(mono),
            sizes,
            mono_size: self.config.font_size,
        })
    }

    /// The player's items, each laid on the frame cut to its region:
    /// recorded anew when the view changed, laid again as they were when it
    /// did not.
    fn paint_plugin_panel_items(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        shown: &mut Shown,
        fonts: &PanelFonts,
        chrome: &UiPalette,
    ) -> anyhow::Result<()> {
        let key = PaintKey {
            revision: shown.player.revision(),
            origin: shown.origin,
            scale: shown.scale,
            window: (self.dimensions.pixel_width, self.dimensions.pixel_height),
            shapes: self.shape_generation,
            chrome: *chrome,
            sizes: fonts.sizes,
            mono_size: fonts.mono_size,
        };
        if shown
            .painted
            .as_ref()
            .is_none_or(|painted| painted.key != key)
        {
            // Let go of the last before the next is recorded.
            shown.painted = None;
            let regions = self.record_plugin_panel(shown, fonts, chrome)?;
            shown.painted = Some(Painted { key, regions });
        }
        let painted = shown.painted.as_ref().expect("recorded above");
        for (clip, heap) in &painted.regions {
            self.lay_plugin_panel_heap(layers, heap, shown, *clip)?;
        }
        Ok(())
    }

    fn record_plugin_panel(
        &self,
        shown: &Shown,
        fonts: &PanelFonts,
        chrome: &UiPalette,
    ) -> anyhow::Result<Vec<(Bounds, HeapQuadAllocator)>> {
        let mut regions: Vec<(Bounds, HeapQuadAllocator)> = Vec::new();
        let mut budget = QUAD_LIMIT;
        for draw in &shown.player.draw() {
            if regions.last().is_none_or(|(clip, _)| *clip != draw.clip) {
                regions.push((draw.clip, HeapQuadAllocator::default()));
            }
            let (_, heap) = regions.last_mut().expect("pushed above");
            let mut recorded = TripleLayerQuadAllocator::Heap(heap);
            self.paint_plugin_panel_draw(&mut recorded, shown, draw, fonts, chrome, &mut budget)?;
        }
        Ok(regions)
    }

    fn lay_plugin_panel_heap(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        heap: &HeapQuadAllocator,
        shown: &Shown,
        clip: Bounds,
    ) -> anyhow::Result<()> {
        let (x, y, scale) = (shown.origin.0, shown.origin.1, shown.scale);
        let clip = QuadClipRect::from_top_left_pixels(
            x + clip.left * scale,
            y + clip.top * scale,
            x + clip.right * scale,
            y + clip.bottom * scale,
            &self.dimensions,
        );
        heap.apply_to_clipped(layers, clip, 1.0)
    }

    fn paint_plugin_panel_draw(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        shown: &Shown,
        draw: &Draw<'_>,
        fonts: &PanelFonts,
        chrome: &UiPalette,
        budget: &mut usize,
    ) -> anyhow::Result<()> {
        let scale = shown.scale;
        // What of the window the draw shows in, across.
        let across = (
            shown.origin.0 + draw.clip.left * scale,
            shown.origin.0 + draw.clip.right * scale,
        );
        let spent = match draw.what {
            Drawn::Rect(_) | Drawn::Hover(_) | Drawn::Thumb(_) => spend(budget, 1),
            Drawn::Text(_) | Drawn::Line(_) | Drawn::Area(_) => true,
        };
        if !spent {
            return Ok(());
        }
        // A box in the item's units, in window pixels.
        let place = |x: f32, y: f32, w: f32, h: f32| -> RectF {
            euclid::rect(
                shown.origin.0 + (x + draw.dx) * scale,
                shown.origin.1 + (y + draw.dy) * scale,
                w.max(0.0) * scale,
                h.max(0.0) * scale,
            )
        };
        match draw.what {
            Drawn::Rect(Rect {
                x,
                y,
                w,
                h,
                fill,
                radius,
                border,
            }) => {
                let fill = fill.map(|fill| color(chrome, fill));
                let border = border.map(|border| color(chrome, border));
                self.paint_panel_rect(layers, place(*x, *y, *w, *h), fill, border, radius * scale)
            }
            Drawn::Hover(hit) => {
                let tint = color(chrome, hit.hover.unwrap_or_default());
                let area = place(hit.x, hit.y, hit.w, hit.h);
                self.paint_panel_rect(layers, area, Some(tint), None, hit.radius * scale)
            }
            Drawn::Text(text) => {
                let area = place(text.x, text.y, text.w, text.h);
                let text_color = color(chrome, text.color);
                self.paint_panel_text(layers, text, area, fonts, text_color, across, budget)
            }
            Drawn::Line(Line {
                points,
                width,
                color: line_color,
            }) => {
                let at = |x: f32, y: f32| {
                    (
                        shown.origin.0 + (x + draw.dx) * scale,
                        shown.origin.1 + (y + draw.dy) * scale,
                    )
                };
                let points: Vec<(f32, f32)> = points
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|[x, y]| at(*x, *y))
                    .collect();
                let line_color = color(chrome, *line_color);
                self.paint_panel_line(layers, &points, width * scale, line_color, across, budget)
            }
            Drawn::Area(Area {
                points,
                base,
                color: fill,
                fade,
            }) => {
                let at = |x: f32, y: f32| {
                    (
                        shown.origin.0 + (x + draw.dx) * scale,
                        shown.origin.1 + (y + draw.dy) * scale,
                    )
                };
                let points: Vec<(f32, f32)> = points
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|[x, y]| at(*x, *y))
                    .collect();
                let base = shown.origin.1 + (base + draw.dy) * scale;
                let fill = color(chrome, *fill);
                self.paint_panel_area(layers, &points, base, fill, *fade, across, budget)
            }
            Drawn::Thumb(thumb) => {
                let area: RectF = euclid::rect(
                    shown.origin.0 + thumb.left * scale,
                    shown.origin.1 + thumb.top * scale,
                    thumb.width() * scale,
                    thumb.height() * scale,
                );
                // A thumb across is as round as one down.
                let radius = area.width().min(area.height()) / 2.0;
                self.paint_panel_rect(layers, area, Some(chrome.scrollbar_thumb), None, radius)
            }
        }
    }

    fn paint_panel_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        area: RectF,
        fill: Option<LinearRgba>,
        border: Option<LinearRgba>,
        radius: f32,
    ) -> anyhow::Result<()> {
        if area.width() <= 0.0 || area.height() <= 0.0 {
            return Ok(());
        }
        let stroke = self.ui_f32(1.0).max(1.0);
        match (fill, border) {
            (Some(fill), Some(border)) => self.fill_rounded_rectangle_with_border(
                layers, LAYER, area, fill, border, radius, stroke,
            ),
            (Some(fill), None) => self.fill_rounded_rectangle(layers, LAYER, area, fill, radius),
            (None, Some(border)) => self.stroke_panel_rect(layers, area, border, radius, stroke),
            (None, None) => Ok(()),
        }
    }

    /// A rounded outline with nothing inside: arcs at the corners, strips
    /// between them.
    fn stroke_panel_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        area: RectF,
        border: LinearRgba,
        radius: f32,
        stroke: f32,
    ) -> anyhow::Result<()> {
        let radius = radius
            .min(area.width() / 2.0)
            .min(area.height() / 2.0)
            .round()
            .max(0.0);
        if radius >= 1.0 {
            let size = euclid::size2(radius, radius);
            for (x, y, polys) in [
                (area.min_x(), area.min_y(), TOP_LEFT_ROUNDED_CORNER_OUTLINE),
                (
                    area.max_x() - radius,
                    area.min_y(),
                    TOP_RIGHT_ROUNDED_CORNER_OUTLINE,
                ),
                (
                    area.min_x(),
                    area.max_y() - radius,
                    BOTTOM_LEFT_ROUNDED_CORNER_OUTLINE,
                ),
                (
                    area.max_x() - radius,
                    area.max_y() - radius,
                    BOTTOM_RIGHT_ROUNDED_CORNER_OUTLINE,
                ),
            ] {
                self.poly_quad(layers, LAYER, euclid::point2(x, y), polys, 0, size, border)?
                    .set_grayscale();
            }
        }
        let edges: [RectF; 4] = [
            euclid::rect(
                area.min_x() + radius,
                area.min_y(),
                area.width() - radius * 2.0,
                stroke,
            ),
            euclid::rect(
                area.min_x() + radius,
                area.max_y() - stroke,
                area.width() - radius * 2.0,
                stroke,
            ),
            euclid::rect(
                area.min_x(),
                area.min_y() + radius,
                stroke,
                area.height() - radius * 2.0,
            ),
            euclid::rect(
                area.max_x() - stroke,
                area.min_y() + radius,
                stroke,
                area.height() - radius * 2.0,
            ),
        ];
        for edge in edges {
            if edge.width() > 0.0 && edge.height() > 0.0 {
                self.filled_rectangle(layers, LAYER, edge, border)?;
            }
        }
        Ok(())
    }

    /// One line of text in its box: centred in its height, placed across
    /// it as asked, and cut to it -- with an ellipsis in the interface
    /// font, at the edge in the monospaced one.
    /// Only the glyphs within `across`, the draw's left and right edges, are
    /// laid: a line far wider than a list scrolled sideways costs what shows.
    #[allow(clippy::too_many_arguments)]
    fn paint_panel_text(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        text: &Text,
        area: RectF,
        fonts: &PanelFonts,
        color: LinearRgba,
        across: (f32, f32),
        budget: &mut usize,
    ) -> anyhow::Result<()> {
        let (clip_left, clip_right) = (area.min_x().max(across.0), area.max_x().min(across.1));
        if text.text.is_empty() || area.width() < 1.0 || clip_right <= clip_left {
            return Ok(());
        }
        let (font, metrics) = fonts.of(text);
        let top = area.min_y() + (area.height() - metrics.cell_size.height as f32) / 2.0;
        let shown = match text.font {
            Font::Ui => self.ellipsize_ui_text(font, &text.text, area.width() as usize)?,
            Font::Mono => std::borrow::Cow::Borrowed(text.text.as_str()),
        };
        if shown.is_empty() {
            return Ok(());
        }
        let (shaped, _) = self.cached_ui_shape(font, metrics, &shown)?;
        let width: f32 = shaped
            .iter()
            .map(|info| info.glyph.x_advance.get() as f32)
            .sum();
        let left = match text.align {
            Align::Left => area.min_x(),
            Align::Center => area.min_x() + (area.width() - width) / 2.0,
            Align::Right => area.max_x() - width,
        }
        .max(area.min_x());
        // The glyphs laid are those that show.
        let mut x = left;
        let showing = shaped
            .iter()
            .filter(|info| {
                let advance = info.glyph.x_advance.get() as f32;
                let shows = x < clip_right && x + advance > clip_left;
                x += advance;
                shows
            })
            .count();
        if !spend(budget, showing) {
            return Ok(());
        }
        match text.font {
            Font::Ui => self.paint_cached_ui_shape_clipped(
                layers,
                metrics,
                &shaped,
                left,
                top,
                clip_left,
                clip_right,
                |_| color,
            ),
            Font::Mono => self.paint_cached_ui_shape_pixel_clipped(
                layers,
                metrics,
                &shaped,
                left,
                top,
                clip_left,
                clip_right,
                |_| color,
            ),
        }
        .map(drop)
    }

    /// A line through `points`, in window pixels, as a column a pixel wide
    /// for each pixel it runs across, tall enough to reach the next: only
    /// those within `across`, the draw's left and right edges.
    #[allow(clippy::too_many_arguments)]
    fn paint_panel_line(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        points: &[(f32, f32)],
        width: f32,
        color: LinearRgba,
        across: (f32, f32),
        budget: &mut usize,
    ) -> anyhow::Result<()> {
        let width = width.max(1.0);
        for pair in points.windows(2) {
            // Walked left to right.
            let ((x0, y0), (x1, y1)) = if pair[0].0 <= pair[1].0 {
                (pair[0], pair[1])
            } else {
                (pair[1], pair[0])
            };
            let run = x1 - x0;
            // A column is widened where the line is steep: one just beyond
            // an edge shows.
            let (from, to) = (x0.max(across.0 - width), x1.min(across.1 + width));
            if from > to {
                continue;
            }
            let y_at = |x: f32| y0 + (y1 - y0) * (x - x0) / run;
            let steps = (to - from).ceil().max(1.0) as usize;
            for step in 0..steps {
                if !spend(budget, 1) {
                    return Ok(());
                }
                let xa = from + (to - from) * step as f32 / steps as f32;
                let xb = from + (to - from) * (step + 1) as f32 / steps as f32;
                // Upright, it is one column from end to end.
                let (ya, yb) = if run > 0.0 {
                    (y_at(xa), y_at(xb))
                } else {
                    (y0, y1)
                };
                let wide = (xb - xa).max(1.0);
                let rise = (yb - ya).abs();
                // Steep, it is widened as well, or it would thin to a pixel.
                let (left, wide) = if rise > wide {
                    (xa - (width - 1.0) / 2.0, wide + width - 1.0)
                } else {
                    (xa, wide)
                };
                let top = ya.min(yb) - width / 2.0;
                let column: RectF = euclid::rect(left, top, wide, rise + width);
                self.filled_rectangle(layers, LAYER, column, color)?;
            }
        }
        Ok(())
    }

    /// What lies between a line through `points` and the level `base`, as
    /// columns a pixel wide, only those within `across`; with `fade`, clear
    /// at `base` and as strong as `color` at the area's highest point.
    #[allow(clippy::too_many_arguments)]
    fn paint_panel_area(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        points: &[(f32, f32)],
        base: f32,
        color: LinearRgba,
        fade: bool,
        across: (f32, f32),
        budget: &mut usize,
    ) -> anyhow::Result<()> {
        let highest = points.iter().map(|(_, y)| *y).fold(base, f32::min);
        let depth = (base - highest).max(1.0);
        for pair in points.windows(2) {
            let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
            let run = x1 - x0;
            let (from, to) = (x0.max(across.0), x1.min(across.1));
            if run <= 0.0 || from >= to {
                continue;
            }
            let steps = (to - from).ceil().max(1.0) as usize;
            for step in 0..steps {
                let left = from + (to - from) * step as f32 / steps as f32;
                let right = from + (to - from) * (step + 1) as f32 / steps as f32;
                let y = y0 + (y1 - y0) * ((left + right) / 2.0 - x0) / run;
                let (top, bottom) = (y.min(base), y.max(base));
                if bottom - top < 0.5 {
                    continue;
                }
                if !spend(budget, 1) {
                    return Ok(());
                }
                let column: RectF = euclid::rect(left, top, (right - left).max(1.0), bottom - top);
                let mut quad = self.filled_rectangle(layers, LAYER, column, color)?;
                if fade {
                    let strength = ((base - top) / depth).clamp(0.0, 1.0);
                    let clear = color.mul_alpha(0.0);
                    if y <= base {
                        quad.set_vertical_gradient(color.mul_alpha(strength), clear);
                    } else {
                        quad.set_vertical_gradient(clear, color.mul_alpha(strength));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Takes `cost` quads from `budget`; none, and false, when it has not that
/// many left.
fn spend(budget: &mut usize, cost: usize) -> bool {
    match budget.checked_sub(cost) {
        Some(left) => {
            *budget = left;
            true
        }
        None => false,
    }
}

/// Whether `domain` runs its programs on this machine, so the directory of
/// one of its panes is a directory here: a plain local terminal, or one the
/// local session server keeps -- not WSL, a serial port or a remote host.
fn runs_here(domain: &Arc<dyn Domain>) -> bool {
    domain
        .downcast_ref::<mux::domain::LocalDomain>()
        .is_some_and(mux::domain::LocalDomain::is_plain_local)
        || domain
            .downcast_ref::<wezterm_client::domain::ClientDomain>()
            .is_some_and(|client| client.is_local_session_host())
}

/// The path a pane's directory `url` names on the other machine it runs on.
fn remote_path(url: &Url) -> Option<String> {
    if url.scheme() != "file" {
        return None;
    }
    let path = percent_encoding::percent_decode_str(url.path())
        .decode_utf8()
        .ok()?;
    (!path.is_empty()).then(|| path.into_owned())
}

/// The path a pane's directory `url` names on this machine.
fn local_path(url: &Url) -> Option<String> {
    if url.scheme() != "file" {
        return None;
    }
    let path = percent_encoding::percent_decode_str(url.path())
        .decode_utf8()
        .ok()?;
    // `/C:/Users/...` on Windows.
    let path = match path.strip_prefix('/') {
        Some(rest) if cfg!(windows) && rest.as_bytes().get(1) == Some(&b':') => rest,
        _ => &path,
    };
    (!path.is_empty()).then(|| path.to_string())
}

/// What the plugin is told of a panel this big, set in `fonts`.
fn panel_env(fonts: &PanelFonts, chrome: &UiPalette, width: f32, height: f32, scale: f32) -> Env {
    let text = |(_, metrics): &(Rc<LoadedFont>, RenderMetrics), size: f64| TextMetrics {
        size: size as f32,
        line: metrics.cell_size.height as f32 / scale,
    };
    let (_, mono) = &fonts.mono;
    Env {
        width: width / scale,
        height: height / scale,
        scale,
        dark: chrome.is_dark(),
        small: text(&fonts.small, fonts.sizes[0]),
        body: text(&fonts.body, fonts.sizes[1]),
        title: text(&fonts.title, fonts.sizes[2]),
        mono: MonoMetrics {
            size: fonts.mono_size as f32,
            line: mono.cell_size.height as f32 / scale,
            advance: mono.cell_size.width as f32 / scale,
        },
        locale: crate::i18n::current_locale().to_string(),
        cwd: None,
        remote: None,
        can_extend: false,
        close: None,
    }
}

/// Sends the plugin what the view's lists need next, once it is told the
/// size they are drawn for.
fn send_wanted(shown: &mut Shown) {
    if shown.env_due {
        return;
    }
    for wanted in shown.player.wanted() {
        let wanted = Raw::new(&wanted);
        plugins::tell_panel(shown.view, PanelRequest::Rows { wanted });
    }
}

/// Tells the plugin a size, fonts or theme of `shown` that changed, the
/// last of those within [`ENV_EVERY`]: the time to come back when it is not
/// time yet.
fn tell_env(shown: &mut Shown, env: Env, now: Instant) -> Option<Instant> {
    shown.env_due |= shown.player.set_env(env);
    if !shown.env_due {
        return None;
    }
    let due = shown.env_told + ENV_EVERY;
    if now < due {
        return Some(due);
    }
    shown.env_told = now;
    shown.env_due = false;
    let env = Raw::new(shown.player.env());
    plugins::tell_panel(shown.view, PanelRequest::Env { env });
    send_wanted(shown);
    None
}

/// What the host said about `shown`, a view of plugin `plugin`. One the
/// host closed but may serve again shows what it last had meanwhile: a
/// panel is opened again in a while, an extended view on its panel's next
/// opening.
fn heard(shown: &mut Shown, plugin: &str, news: PanelNews) {
    match news {
        PanelNews::Frame(frame) => {
            shown.player.frame(frame);
            shown.status = Status::Open;
            plugins::tell_panel(shown.view, PanelRequest::Shown);
            send_wanted(shown);
        }
        PanelNews::Rows(rows) => {
            shown.player.rows(rows);
            send_wanted(shown);
        }
        PanelNews::Closed { reason, again } => {
            shown.status = if again {
                Status::Again(Instant::now() + REOPEN_AFTER)
            } else {
                log::info!("plugin panel {plugin}: {reason}");
                Status::Stopped(reason)
            };
        }
        PanelNews::Unreachable(why) => {
            shown.status = Status::Lost(why);
        }
        // An extended view's: it goes with its panel, which is opened again.
        PanelNews::Reconnected => {}
        // Answered by the window, before it gets here.
        PanelNews::Remote { .. } => {}
    }
}

/// Does `ask` on the machine `reach` names, and says what came of it.
async fn ask_there(reach: Reach, ask: Ask) -> Answer {
    let lease = match remote_connection_manager()
        .acquire(reach.connection_key, reach.config, true)
        .await
    {
        Ok(lease) => lease,
        Err(RemoteAcquireError::Failed(why)) => return Answer::failed(why),
        Err(RemoteAcquireError::NotConnected) => {
            return Answer::failed(format!("ThinkTerm is not connected to {}", reach.host))
        }
    };
    let backend = lease.backend();
    let within = |limit: u64| usize::try_from(limit).unwrap_or(usize::MAX).min(ASK_BYTES);
    match ask {
        Ask::Run { args, cwd, limit } => {
            let line = run_command_line(&cwd, &args);
            match backend.run(line, within(limit), ASK_RUN_WAIT).await {
                Ok(ran) => Answer::Ran {
                    status: ran.status.and_then(|status| i32::try_from(status).ok()),
                    out: Bytes(ran.out),
                    cut: ran.cut,
                },
                Err(why) => Answer::failed(why),
            }
        }
        Ask::Read { path, limit } => {
            let path = match RemotePath::from_server_absolute(&path) {
                Ok(path) => path,
                Err(why) => return Answer::failed(why),
            };
            match backend.read_file(path, within(limit)).await {
                Ok(read) => Answer::Read {
                    bytes: Bytes(read.bytes),
                    cut: read.truncated,
                },
                Err(why) => Answer::failed(why),
            }
        }
        Ask::Stat { path } => match backend.stat(path).await {
            Ok(found) => Answer::Stat {
                entry: found.map(|stat| Entry {
                    kind: match stat.kind {
                        RemoteFileKind::File => EntryKind::File,
                        RemoteFileKind::Directory => EntryKind::Dir,
                        RemoteFileKind::Symlink => EntryKind::Link,
                        RemoteFileKind::Other => EntryKind::Other,
                    },
                    len: stat.size,
                    modified: stat.modified,
                    target: stat.target,
                }),
            },
            Err(why) => Answer::failed(why),
        },
    }
}

/// The word a plugin is told the connection `connection_key` names its
/// machine by. The key holds a digest of the password, so it is hashed
/// again under a key of this process's own: nothing of it leaves, and the
/// word is the same for every window.
fn machine_word(connection_key: &str) -> String {
    static KEY: OnceLock<RandomState> = OnceLock::new();
    format!(
        "{:016x}",
        KEY.get_or_init(RandomState::new).hash_one(connection_key)
    )
}

/// The machine an SSH address names, without the user or the port.
fn host_name(address: &str) -> String {
    let address = address.rsplit_once('@').map_or(address, |(_, host)| host);
    if let Some((host, _)) = address
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
    {
        return host.to_string();
    }
    match address.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => {
            host.to_string()
        }
        _ => address.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_is_its_path_here() {
        let url = |text: &str| Url::parse(text).unwrap();
        assert_eq!(
            local_path(&url("file://devbox/home/user/my%20project")).as_deref(),
            Some("/home/user/my project")
        );
        assert_eq!(local_path(&url("https://example.com/a")), None);
        assert_eq!(
            local_path(&url("file://devbox")).as_deref(),
            Some("/"),
            "the root"
        );
    }

    #[test]
    fn a_remote_directory_is_its_path_there_and_a_host_its_name() {
        let url = Url::parse("file://server-a/C:/work/my%20app").unwrap();
        assert_eq!(
            remote_path(&url).as_deref(),
            Some("/C:/work/my app"),
            "as it is there"
        );
        assert_eq!(host_name("devbox"), "devbox");
        assert_eq!(host_name("user@devbox:2222"), "devbox");
        assert_eq!(host_name("[fe80::1]:22"), "fe80::1");
        assert_eq!(host_name("fe80::1"), "fe80::1");
    }

    #[test]
    fn a_unit_is_a_point() {
        let retina = if cfg!(target_os = "macos") { 144 } else { 192 };
        assert_eq!(panel_scale(retina), 2.0);
        assert_eq!(panel_scale(0), panel_scale(1));
    }
}
