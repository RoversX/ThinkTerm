//! ThinkTerm Library: every Space the window can reach, one tall card each
//! -- the Space's own sidebar, painted as the sidebar paints it -- grouped by
//! machine, in a panel that takes the left part of the window while the
//! terminal keeps the rest.
//!
//! It opens from the sidebar's footer. The Library comes out from the left
//! edge and, in the same motion, the sidebar swings out over the terminal and
//! round into its own card; going back runs the other way. As with a
//! sidebar's slide (see `sidebar_slide`), the terminal is laid out for where
//! it ends up the moment the Library opens or closes, and only its picture
//! moves on the way, so its programs hear of one resize. The same goes for
//! dragging the Library's edge: the terminal takes the new width when the
//! drag ends.

use crate::quad::{HeapQuadAllocator, HeapQuadMark, QuadClipRect, TripleLayerQuadAllocator};
use crate::termwindow::sidebar_slide::{Out, Panel, Side, SidebarSlideFrame};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::ui::sidebar::{
    SpaceConnectionState, WorkspaceSidebarRect, SIDEBAR_SETTINGS_FOOTER_HEIGHT,
};
use crate::termwindow::ui::tokens::{SIDEBAR_INSET, SIDEBAR_MIN_WIDTH};
use crate::termwindow::{TermWindowNotif, UIItem, UIItemType};
use crate::ui::draw::DrawContext;
use crate::ui::scale_ui_usize;
use crate::utilsprites::RenderMetrics;
use crate::workspace_threads::{self, SpaceView};
use anyhow::Context;
use config::keyassignment::KeyAssignment;
use fluent_bundle::FluentArgs;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wezterm_font::LoadedFont;
use window::color::LinearRgba;
use window::{
    ContextMenuIcon, ContextMenuItem, MouseCursor, MouseEvent, MouseEventKind as WMEK, MousePress,
    RectF, ScrollPhase, WindowOps,
};

/// The least of the terminal that stays in view beside the Library, in
/// design pixels: the edge goes no further.
const TERMINAL_LEAST: f32 = 320.0;

/// The panel's parts, in design pixels (2x backing pixels, see `ui_scale_for_dpi`).
const RAIL_WIDTH: f32 = 144.0;
/// What floats down the left: the Spaces tab -- its icon in a pill, its name
/// shown on hover -- and round buttons below.
const TAB_WIDTH: f32 = 104.0;
const TAB_HEIGHT: f32 = 64.0;
const ROUND_BUTTON: f32 = 64.0;
const ROUND_BUTTON_GAP: f32 = 12.0;
const PADDING: f32 = 32.0;
const TITLE_HEIGHT: f32 = 52.0;
const CARD_GAP: f32 = 32.0;
const MACHINE_GAP: f32 = 60.0;
const MACHINE_HEAD: f32 = 48.0;
const CARD_RADIUS: f32 = 26.0;
/// How far in from a card's top its sidebar's Space row sits, and from its
/// bottom its list ends: clear of the rounded corners.
const CARD_INSET: usize = 2 * SIDEBAR_INSET;
/// How wide the shadow an edge casts over the row of cards running under it.
const EDGE_SHADOW: f32 = 24.0;
const EDGE_HANDLE: f32 = 16.0;
/// How far right of its card the sidebar swings on its way in, and the least
/// it swings out from the window's edge.
const SWING: f32 = 460.0;
const SWING_LEAST: f32 = 600.0;
/// How far the edge gives, pulled past where it may go.
const EDGE_GIVE: f32 = 220.0;
/// The blur of the shadow under the sidebar on its way.
const FLIGHT_SHADOW: f32 = 16.0;

/// What is on screen is read again this often while the Library stands
/// still: a server connecting, a thread starting, a Space made elsewhere.
const FRESH_FOR: Duration = Duration::from_secs(1);
/// How many cards a frame paints afresh, while the Library moves and while
/// it stands still.
const FRESH_MOVING: usize = 1;
const FRESH_STILL: usize = 2;

/// How a spring moves: its period in seconds, and its damping -- 1 comes to
/// rest without passing where it goes, lower comes back from a little past.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Feel {
    response: f32,
    zeta: f32,
}

/// The Library coming out from the left edge, and the terminal making room.
const COME_OUT: Feel = Feel {
    response: 0.34,
    zeta: 0.92,
};
/// The sidebar on its way into its card: one curve, ending a little past.
const LAND: Feel = Feel {
    response: 0.50,
    zeta: 0.78,
};
/// Going back: nothing past, and slowing well before it gets there -- as
/// quick to halfway as a critically damped spring, but a longer last part.
const GO_BACK: Feel = Feel {
    response: 0.30,
    zeta: 1.4,
};
/// The row of cards bringing a chosen card into view as it goes back:
/// quicker than the flight, so the card is out from under the terminal
/// early on its way.
const GLIDE: Feel = Feel {
    response: 0.26,
    zeta: 1.0,
};
/// The edge, let go past where it may go, springing back inside.
const SNAP: Feel = Feel {
    response: 0.42,
    zeta: 0.58,
};
/// Going back, the card lifts out first and the Library follows it this much
/// later, in seconds.
const GO_BACK_LAG: f32 = 0.12;
/// Let go at this speed or more, in pixels a second, towards where the
/// finger was taking it, the Library goes on there however far it got.
const LET_GO_FLING: f32 = 600.0;
/// Springs are stepped this finely, and a frame is never taken for longer
/// than the most, so a stall cannot fling one.
const STEP: f32 = 1.0 / 240.0;
const MOST_STEP: f32 = 0.05;
/// A run of frames ends after this many, whatever is still moving.
const MOST_PAINTS: usize = 240;

#[derive(Clone, Copy, Debug, Default)]
struct Spring {
    x: f32,
    v: f32,
    target: f32,
    response: f32,
    zeta: f32,
    delay: f32,
}

impl Spring {
    fn to(&mut self, target: f32, feel: Feel, delay: f32) {
        self.target = target;
        self.response = feel.response;
        self.zeta = feel.zeta;
        self.delay = delay;
    }

    fn settle(&mut self) {
        self.x = self.target;
        self.v = 0.0;
        self.delay = 0.0;
    }

    fn moving(&self) -> bool {
        self.delay > 0.0 || (self.x - self.target).abs() > 0.0005 || self.v.abs() > 0.005
    }

    fn advance(&mut self, dt: f32) {
        if !self.moving() || self.response <= 0.0 {
            self.settle();
            return;
        }
        let mut left = dt;
        while left > 0.0 {
            let h = left.min(STEP);
            left -= h;
            if self.delay > 0.0 {
                self.delay -= h;
                continue;
            }
            let w = std::f32::consts::TAU / self.response;
            self.v += (-w * w * (self.x - self.target) - 2.0 * self.zeta * w * self.v) * h;
            self.x += self.v * h;
        }
        if !self.moving() {
            self.settle();
        }
    }
}

/// The edge, dragged or springing back inside where it may go.
#[derive(Clone, Copy, Debug)]
struct Edge {
    /// Where the pointer took it, and the width it had then.
    grab: isize,
    from: f32,
    /// Its width on screen, ahead of the terminal's.
    width: f32,
    /// Let go outside its range: from where, to where, and how far along.
    back: Option<(f32, f32)>,
    spring: Spring,
}

/// The Library going with a finger on the trackpad, out from the first
/// Space or back from beside its cards: as far along as the finger has
/// gone, its springs left where it puts them until it lets go.
#[derive(Clone, Copy, Debug)]
struct Held {
    opening: bool,
}

/// A swipe begun beside the cards -- the rail, the title, between and
/// under them: sideways towards the terminal it takes the Library back.
#[derive(Clone, Copy, Debug)]
struct Swipe {
    x: f32,
    y: f32,
    /// Towards the terminal, in pixels a second.
    velocity: f32,
    last: Instant,
    sideways: bool,
}

/// The row of cards on its way to show in full a card chosen from it, as
/// the card goes back.
#[derive(Clone, Copy, Debug)]
struct Glide {
    from: f32,
    to: f32,
    spring: Spring,
}

/// What a click in the Library does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LibraryItem {
    /// The sidebar footer's button.
    Open,
    Back,
    /// The Spaces tab: nothing to do yet, its name shown on hover.
    Spaces,
    Background,
    Edge,
    /// A card: go to that Space.
    Space(String),
    Thread {
        space: String,
        thread: String,
    },
    /// A server that is not connected, from one of its cards.
    Connect(String),
    /// A machine's +, for a new Space there; `None` for this computer.
    NewSpace(Option<String>),
    /// A card's ⋯.
    SpaceMenu(String),
}

#[derive(Default)]
pub(crate) struct LibraryState {
    /// Whether the terminal is laid out beside the Library rather than the
    /// sidebar.
    open: bool,
    /// Its width in design pixels, once its edge was dragged; it keeps it
    /// however the window is sized.
    width: Option<f32>,
    /// 0 with the sidebar, 1 with the Library all the way out.
    out: Spring,
    /// The sidebar on its way into its card: 0 docked, 1 in the card.
    landing: Spring,
    /// The terminal's left edge without the Library, from the window's left
    /// border: where it goes back to.
    sidebar_width: f32,
    /// How much of the sidebar is out where the Library comes out of it or
    /// goes back to: 1 docked, 0 folded away, between for one the pointer
    /// was bringing out.
    sidebar_out: f32,
    edge: Option<Edge>,
    held: Option<Held>,
    swipe: Option<Swipe>,
    /// After a swipe that took the Library back or failed to: its momentum
    /// is not for the cards.
    swipe_momentum: bool,
    /// How far the row of cards is scrolled.
    scroll: f32,
    glide: Option<Glide>,
    /// A card going back from out of view -- under the terminal, or
    /// scrolled past the panel's left edge -- is cut to where the row of
    /// cards shows, left and right, and comes out from under what covered
    /// it, until it is wholly inside.
    flight_cut: Option<(f32, f32)>,
    /// Bring the window's own card into view at the next paint.
    reveal: bool,
    last_step: Option<Instant>,
    paints: usize,
    model: Option<LibraryModel>,
    /// The sidebar on its way, recorded where the Library is painted and
    /// drawn over the tab row once that is.
    flight: Option<HeapQuadAllocator>,
    /// While a sidebar is painted for the Library: where it is laid out, and
    /// where the footer a card goes without begins.
    card: Option<WorkspaceSidebarRect>,
    card_footer: Option<HeapQuadMark>,
    /// And where its toolbar, which a card goes without too, starts and ends.
    card_toolbar: Option<(HeapQuadMark, HeapQuadMark)>,
    /// Each card's sidebar as last painted, by Space, so a card is painted
    /// again only once what it shows may have changed. Holds the cards in
    /// view at most, and goes with the Library.
    recordings: HashMap<String, Recording>,
    /// How many more cards this frame may paint afresh; the rest wait for
    /// the next, as they were or empty, so cards coming into view or due
    /// again cost a frame no more than this many sidebars.
    fresh: usize,
}

struct LibraryModel {
    machines: Vec<Machine>,
    built: Instant,
}

struct Machine {
    /// The server's mux domain; `None` for this computer.
    domain: Option<String>,
    label: String,
    state: SpaceConnectionState,
    spaces: Vec<SpaceView>,
}

/// A sidebar painted for the Library, and what it would have registered for
/// the pointer.
struct SidebarRecording {
    quads: HeapQuadAllocator,
    /// Its footer and its toolbar, set apart.
    footer: HeapQuadAllocator,
    toolbar: HeapQuadAllocator,
    items: Vec<UIItem>,
    /// When something on it moves next: a working thread's mark.
    due: Option<Instant>,
}

/// A card's sidebar, recorded with its panel's top left at `at`.
struct Recording {
    key: RecordingKey,
    at: (f32, f32),
    sidebar: SidebarRecording,
}

/// What a card's recording was made for; any change paints it again.
#[derive(Clone, Copy, Debug, PartialEq)]
struct RecordingKey {
    /// The model it was painted from: read again every `FRESH_FOR`, and
    /// whenever a click changes what the window shows.
    built: Instant,
    width: usize,
    height: usize,
    scroll: f32,
    /// The pointer over the card, from its panel's top left.
    pointer: Option<(isize, isize)>,
}

/// Where a card's sidebar is laid out: its top left, which may be off the
/// window, and its size.
#[derive(Clone, Copy, Debug)]
struct CardPanel {
    x: f32,
    y: f32,
    width: usize,
    height: usize,
}

impl CardPanel {
    /// Where to record it -- the painters take no negative pixels -- and
    /// how far to move what is recorded there.
    fn recorded_at(&self) -> ((f32, f32), (f32, f32)) {
        let at = (self.x.max(0.0).round(), self.y.max(0.0).round());
        (at, (self.x - at.0, self.y - at.1))
    }

    fn rect_at(&self, at: (f32, f32)) -> WorkspaceSidebarRect {
        WorkspaceSidebarRect {
            x: at.0 as usize,
            y: at.1 as usize,
            width: self.width,
            height: self.height,
        }
    }
}

/// The Spaces by machine: this computer's first, in their order, then each
/// server's, the servers in the order they first appear -- the order the
/// Space menu lists them in. A server shown as folders in a Space of this
/// computer has no cards, but for a Space of its the window is in.
fn machines(spaces: Vec<SpaceView>) -> Vec<(Option<String>, Vec<SpaceView>)> {
    let mut machines: Vec<(Option<String>, Vec<SpaceView>)> = vec![(None, vec![])];
    for space in spaces {
        if space.shown_as_folders && !space.is_active {
            continue;
        }
        match machines
            .iter_mut()
            .find(|(domain, _)| *domain == space.domain)
        {
            Some((_, spaces)) => spaces.push(space),
            None => machines.push((space.domain.clone(), vec![space])),
        }
    }
    machines.retain(|(_, spaces)| !spaces.is_empty());
    machines
}

/// Where each machine's cards start along the row, from where the row
/// starts, and how long the row is.
fn row_layout(cards_per_machine: &[usize], card: f32, card_gap: f32, machine_gap: f32) -> (Vec<Vec<f32>>, f32) {
    let mut x = 0.0;
    let mut starts = vec![];
    for (index, count) in cards_per_machine.iter().enumerate() {
        if index > 0 {
            x += machine_gap;
        }
        let mut cards = vec![];
        for card_index in 0..*count {
            if card_index > 0 {
                x += card_gap;
            }
            cards.push(x);
            x += card;
        }
        starts.push(cards);
    }
    (starts, x)
}

/// Where the sidebar is along its way into its card, for `along` from 0
/// (docked at `start`) to 1 (at `card`): out towards `swing` and round,
/// without stopping. Past 1 it carries on the way it was going, a little.
fn swing_x(start: f32, swing: f32, card: f32, along: f32) -> f32 {
    if along <= 1.0 {
        let back = 1.0 - along;
        back * back * start + 2.0 * back * along * swing + along * along * card
    } else {
        card + 2.0 * (card - swing) * (along - 1.0)
    }
}

/// `raw` held inside `least..=most`, giving more grudgingly the further it is
/// pulled past, up to `give`.
fn pulled(raw: f32, least: f32, most: f32, give: f32) -> f32 {
    let resisted = |past: f32| (1.0 - 1.0 / (past / give * 0.6 + 1.0)) * give;
    if raw > most {
        most + resisted(raw - most)
    } else if raw < least {
        least - resisted(least - raw)
    } else {
        raw
    }
}

/// How deep the shadow an edge casts over the row of cards running `past`
/// it by: none on a row that fits, all of it once the row runs a shadow's
/// `width` past.
fn overflow_strength(past: f32, width: f32) -> f32 {
    clamp01(past / width.max(1.0))
}

fn clamp01(value: f32) -> f32 {
    value.clamp(0.0, 1.0)
}

fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

fn with_name(id: &str, name: &str) -> String {
    let mut args = FluentArgs::new();
    args.set("name", name.to_string());
    crate::i18n::tr_args(id, &args)
}

/// The fonts and colours a paint uses, gathered once.
struct Look {
    bold: Rc<LoadedFont>,
    bold_metrics: RenderMetrics,
    small: Rc<LoadedFont>,
    small_metrics: RenderMetrics,
    /// What the cards stand on, as the terminal stands beside the sidebar.
    ground: LinearRgba,
    /// The rail, and a card: the sidebar's own colour.
    sidebar: LinearRgba,
    /// What floats over the ground -- the Spaces tab, the round buttons --
    /// its edge, under the pointer, and its shadow.
    float: LinearRgba,
    float_edge: LinearRgba,
    float_hover: LinearRgba,
    shadow: LinearRgba,
    /// What an edge casts over the cards running under it, at its deepest.
    edge_shadow: LinearRgba,
    text: LinearRgba,
    muted: LinearRgba,
    faint: LinearRgba,
    line: LinearRgba,
    accent: LinearRgba,
    /// A button on the panel's ground under the pointer.
    button_hover: LinearRgba,
    danger: LinearRgba,
}

/// Where the panel's parts go this paint, in window pixels.
struct Geometry {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    rail: f32,
    title_y: f32,
    /// Where the row of cards stands: right of the rail, where it starts
    /// and ends scrolled either way.
    cards: RectF,
    /// Where it shows: on under the rail, which floats over it, to the
    /// panel's left edge.
    view: RectF,
    head_y: f32,
    card_y: f32,
    card_width: f32,
    card_height: f32,
}

impl crate::TermWindow {
    /// Whether the Library is on screen: open, or on its way out or back.
    pub(crate) fn library_presented(&self) -> bool {
        let library = &self.library;
        library.open
            || library.out.x > 0.0
            || library.out.moving()
            || library.landing.x > 0.0
            || library.landing.moving()
    }

    pub(crate) fn library_open(&self) -> bool {
        self.library.open
    }

    /// Set while a sidebar is painted for the Library, so its paint is the
    /// sidebar's own.
    pub(crate) fn library_painting_sidebar(&self) -> bool {
        self.library.card.is_some()
    }

    /// Where the sidebar painted for the Library is laid out.
    pub(crate) fn library_card_rect(&self) -> Option<WorkspaceSidebarRect> {
        self.library.card
    }

    /// Where the footer of the sidebar painted for the Library begins.
    pub(crate) fn mark_library_card_footer(&mut self, mark: Option<HeapQuadMark>) {
        self.library.card_footer = mark;
    }

    /// Where the toolbar of the sidebar painted for the Library -- full
    /// screen, its toggle over the Space row -- starts and ends.
    pub(crate) fn mark_library_card_toolbar(&mut self, start: HeapQuadMark, end: HeapQuadMark) {
        self.library.card_toolbar = Some((start, end));
    }

    /// Let go of the cards as recorded: they point into the glyph atlas,
    /// and a recreated atlas has other things where they point.
    pub(crate) fn discard_library_recordings(&mut self) {
        self.library.recordings.clear();
        self.library.flight = None;
    }

    /// The width the terminal is laid out against while the Library is open.
    pub(crate) fn library_laid_out_width(&self) -> Option<usize> {
        self.library.open.then(|| self.library_width() as usize)
    }

    /// How far the open Library pushes what stands beside the sidebar -- the
    /// terminal, its tab row and pane bars and the buttons at their ends, the
    /// right sidebar: all of it keeps its size and goes past the window's
    /// edge by this much.
    pub(crate) fn library_push(&self) -> usize {
        self.library_laid_out_width()
            .map_or(0, |width| width.saturating_sub(self.workspace_sidebar_sizing_width()))
    }

    /// As `library_push`, but where it is drawn this frame: while the
    /// Library comes out or goes back, on its way along with the terminal.
    pub(crate) fn library_drawn_push(&self) -> usize {
        let push = self.library_push() as f32;
        if !self.library_presented() || !self.library_moving() {
            return push as usize;
        }
        // The terminal's shift in `library_slide_frame`.
        let shift = self.library_terminal_edge().round() - self.workspace_sidebar_width() as f32;
        (push + shift).max(0.0) as usize
    }

    fn library_available_width(&self) -> f32 {
        let border = self.get_os_border();
        self.dimensions
            .pixel_width
            .saturating_sub((border.left + border.right).get() as usize) as f32
    }

    /// How narrow and how wide the edge may go: room for one card at least,
    /// and never so wide that less than a strip of the terminal shows.
    fn library_edge_range(&self) -> (f32, f32) {
        let available = self.library_available_width();
        let card = self.workspace_sidebar_docked_width() as f32;
        let least = (self.ui_f32(RAIL_WIDTH) + 2.0 * self.ui_f32(PADDING) + card)
            .max(scale_ui_usize(SIDEBAR_MIN_WIDTH, self.dimensions.dpi) as f32);
        let most = (available - self.ui_f32(TERMINAL_LEAST)).max(least).min(available);
        (least.min(most), most)
    }

    /// Its width: as its edge was left, or room for two cards side by side.
    /// Fixed, as the sidebar's is, whatever the window's.
    fn library_width(&self) -> f32 {
        let (least, most) = self.library_edge_range();
        let width = match self
            .library
            .width
            .or_else(|| crate::native_settings::library_width().map(|width| width as f32))
        {
            Some(design) => self.ui_f32(design),
            None => {
                let card = self.workspace_sidebar_docked_width() as f32;
                self.ui_f32(RAIL_WIDTH)
                    + 2.0 * self.ui_f32(PADDING)
                    + 2.0 * card
                    + self.ui_f32(CARD_GAP)
            }
        };
        width.round().clamp(least, most).max(1.0)
    }

    /// Its width on screen: where its edge is dragged to, or its width.
    fn library_shown_width(&self) -> f32 {
        match &self.library.edge {
            Some(edge) => edge.width,
            None => self.library_width(),
        }
    }

    /// Where the terminal's left edge is on screen, from the window's left
    /// border.
    fn library_terminal_edge(&self) -> f32 {
        let library = &self.library;
        lerp(
            library.sidebar_width,
            self.library_shown_width(),
            library.out.x,
        )
        .max(0.0)
    }

    fn library_moving(&self) -> bool {
        let library = &self.library;
        library.out.moving()
            || library.landing.moving()
            || library.edge.is_some()
            || library.glide.is_some()
            || library.held.is_some()
    }

    /// A window out of sight, drawn as boxes, under a view covering the
    /// whole window -- the Library with it -- or between the terminal and a
    /// view gets the Library where it goes at once. A view in the terminal's
    /// place goes along with the Library's edge as the terminal does.
    fn library_drawable(&self) -> bool {
        self.occluded.is_none()
            && !self.config.use_box_model_render
            && !(self.content_view_foreground() && self.content_view_is_full_window())
            && self.content_view_fade.is_none()
    }

    pub(crate) fn toggle_library(&mut self, context: &dyn WindowOps) {
        if self.library.open {
            self.close_library(context);
        } else {
            self.open_library(context);
        }
    }

    pub(crate) fn open_library(&mut self, context: &dyn WindowOps) {
        if self.library.open {
            return;
        }
        self.finish_sidebar_slides();
        let sidebar_width = self.workspace_sidebar_width() as f32;
        // Folded away but brought out by the pointer, it floats where it
        // docks, or part way there: the flight sets off from where it is,
        // and the reveal is done with.
        let sidebar_out = if self.workspace_sidebar_collapsed {
            let out = self.workspace_sidebar_hover.progress(Instant::now()).unwrap_or(0.0);
            self.workspace_sidebar_hover.cancel_immediately();
            out
        } else {
            1.0
        };
        let drawable = self.library_drawable();
        let library = &mut self.library;
        library.open = true;
        library.sidebar_width = sidebar_width;
        library.sidebar_out = sidebar_out;
        library.glide = None;
        library.flight_cut = None;
        library.model = None;
        library.reveal = true;
        library.paints = 0;
        library.out.to(1.0, COME_OUT, 0.0);
        library.landing.to(1.0, LAND, 0.0);
        if !drawable {
            library.out.settle();
            library.landing.settle();
        }
        // The titlebar's sidebar button has no sidebar to toggle while the
        // Library is in its place.
        self.titlebar_sidebar_button_hovered = false;
        context.set_titlebar_sidebar_button_visible(false);
        // The terminal takes its new place at once; only its picture moves.
        self.reflow_workspace_sidebar(context);
        context.invalidate();
    }

    pub(crate) fn close_library(&mut self, context: &dyn WindowOps) {
        if !self.library.open {
            return;
        }
        self.library.held = None;
        if self.library.edge.is_some() {
            self.commit_library_edge();
        }
        // The window's own card out of view, scrolled away: the row brings
        // it in as it goes back, rather than it flying in from where it was
        // -- across the terminal, or in from past the window's edge.
        let reveal = if self.library_moving() {
            None
        } else {
            self.library_glide_to(&self.active_space_id.clone())
        };
        self.library.open = false;
        self.library.glide = reveal.map(|(glide, _)| glide);
        self.library.flight_cut = reveal.map(|(_, cut)| cut);
        let sidebar_width = self.workspace_sidebar_width() as f32;
        let sidebar_out = if self.workspace_sidebar_collapsed { 0.0 } else { 1.0 };
        let drawable = self.library_drawable();
        let library = &mut self.library;
        library.sidebar_width = sidebar_width;
        library.sidebar_out = sidebar_out;
        library.paints = 0;
        library.landing.to(0.0, GO_BACK, 0.0);
        library.out.to(0.0, GO_BACK, GO_BACK_LAG);
        if !drawable {
            library.out.settle();
            library.landing.settle();
        }
        context.set_titlebar_sidebar_button_visible(true);
        self.reflow_workspace_sidebar(context);
        context.invalidate();
    }

    /// A finger on the trackpad takes the Library out from the first Space
    /// (`opening`), or back from beside its cards, as far as `pull` pixels
    /// of the way the terminal's edge goes: the edge keeps under it.
    pub(crate) fn hold_library(&mut self, opening: bool, pull: f32, context: &dyn WindowOps) {
        if self.library.held.is_none() {
            if opening {
                if self.library.open {
                    return;
                }
                self.open_library(context);
            } else if !self.library.open || self.library_moving() {
                return;
            }
            if !self.library.open || !self.library_drawable() {
                return;
            }
            if !opening {
                // The window's own card out of view: the row brings it in
                // as the finger takes the Library back.
                if let Some((glide, cut)) = self.library_glide_to(&self.active_space_id.clone()) {
                    self.library.glide = Some(glide);
                    self.library.flight_cut = Some(cut);
                }
            }
            self.library.held = Some(Held { opening });
        }
        let along = clamp01(pull / self.library_reach());
        let x = if opening { along } else { 1.0 - along };
        for spring in [&mut self.library.out, &mut self.library.landing] {
            spring.x = x;
            spring.v = 0.0;
            spring.delay = 0.0;
        }
        context.invalidate();
    }

    /// The finger lets go: the Library goes on to where it was being
    /// taken, if it got halfway or was flung that way, else back; at
    /// `velocity` pixels a second towards where it was being taken.
    pub(crate) fn let_go_library(&mut self, velocity: f32, context: &dyn WindowOps) {
        let Some(held) = self.library.held.take() else {
            return;
        };
        let x = self.library.out.x;
        let along = if held.opening { x } else { 1.0 - x };
        let goes_on = along >= 0.5 || (velocity >= LET_GO_FLING && along > 0.02);
        // Out of the finger's speed into the springs': the way it was going
        // is towards 1 opening, towards 0 going back. Left where it began,
        // it has nothing to carry on with.
        let speed = if along > 0.02 {
            velocity / self.library_reach() * if held.opening { 1.0 } else { -1.0 }
        } else {
            0.0
        };
        if held.opening != goes_on {
            self.close_library(context);
            // From where the finger left it, together: nothing has lifted
            // out first for the Library to follow.
            self.library.out.to(0.0, GO_BACK, 0.0);
        } else {
            self.library.out.to(1.0, COME_OUT, 0.0);
            self.library.landing.to(1.0, LAND, 0.0);
        }
        self.library.paints = 0;
        for spring in [&mut self.library.out, &mut self.library.landing] {
            spring.v = speed;
        }
        context.invalidate();
    }

    /// How far the terminal's edge goes between the sidebar and the
    /// Library, which is how far a finger takes the Library all the way.
    fn library_reach(&self) -> f32 {
        (self.library_shown_width() - self.library.sidebar_width).max(1.0)
    }

    /// Let go of what the Library holds while it is on screen.
    fn release_library(&mut self) {
        let library = &mut self.library;
        library.last_step = None;
        library.paints = 0;
        library.model = None;
        library.flight = None;
        library.glide = None;
        library.flight_cut = None;
        library.held = None;
        library.swipe = None;
        library.swipe_momentum = false;
        library.recordings.clear();
    }

    /// Move the Library along. Called once a frame, before anything reads
    /// where it is.
    pub(crate) fn advance_library(&mut self, now: Instant) {
        if !self.library_presented() {
            // Gone, however it went -- closed out of sight it never moved.
            self.release_library();
            return;
        }
        // A hold whose finger has gone -- its gesture cancelled, or taken
        // over with no end -- lets go where the finger left it.
        if let Some(held) = self.library.held {
            let finger = if held.opening {
                self.workspace_sidebar_swipe.library_pull().is_some()
            } else {
                self.library.swipe.is_some_and(|swipe| swipe.sideways)
            };
            if !finger {
                if let Some(window) = self.window.clone() {
                    self.let_go_library(0.0, &window);
                }
            }
        }
        if !self.library_moving() {
            self.library.last_step = None;
            self.library.paints = 0;
            return;
        }
        let dt = self
            .library
            .last_step
            .map(|at| now.saturating_duration_since(at).as_secs_f32())
            .unwrap_or(0.0)
            .min(MOST_STEP);
        let drawable = self.library_drawable();
        let library = &mut self.library;
        library.last_step = Some(now);
        library.paints += 1;
        if library.held.is_some() {
            // Where the finger has it, for as long as it takes.
            library.paints = 0;
        }
        let give_up = !drawable || library.paints > MOST_PAINTS;
        for spring in [&mut library.out, &mut library.landing] {
            if library.held.is_some() {
                continue;
            }
            if give_up {
                spring.settle();
            } else {
                spring.advance(dt);
            }
        }
        if let Some(glide) = library.glide.as_mut() {
            if give_up {
                glide.spring.settle();
            } else {
                glide.spring.advance(dt);
            }
            library.scroll = lerp(glide.from, glide.to, glide.spring.x);
            if !glide.spring.moving() {
                library.glide = None;
            }
        }
        let mut edge_home = false;
        if let Some(edge) = library.edge.as_mut() {
            if let Some((from, to)) = edge.back {
                if give_up {
                    edge.spring.settle();
                } else {
                    edge.spring.advance(dt);
                }
                edge.width = lerp(from, to, edge.spring.x);
                edge_home = !edge.spring.moving();
            }
        }
        if edge_home {
            // Back inside: that is where the terminal goes, laid out once the
            // frame is over.
            if let Some(window) = self.window.as_ref() {
                window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                    term_window.commit_library_edge();
                })));
            }
        }
        if !self.library_presented() {
            self.release_library();
        } else if self.library_moving() {
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }
    }

    /// While the Library comes out or goes back, or its edge is dragged:
    /// the terminal is laid out where it goes and drawn where it is. It is
    /// as wide as beside the sidebar either way, so it reaches past the
    /// window's edge and leaves no room it does not cover.
    pub(crate) fn library_slide_frame(&self, now: Instant) -> Option<SidebarSlideFrame> {
        if !self.library_presented() || !self.library_moving() {
            return None;
        }
        let border = self.get_os_border();
        let edge = self.library_terminal_edge();
        let laid_out = self.workspace_sidebar_width() as f32;
        // Wider than either, so nothing counts as past where it docks.
        let docked = edge.max(laid_out) + 1.0;
        SidebarSlideFrame::new(
            border.left.get() as f32,
            self.dimensions
                .pixel_width
                .saturating_sub(border.right.get() as usize) as f32,
            self.dimensions.pixel_height as f32,
            Panel {
                docked,
                laid_out,
                out: Some(Out::along(edge / docked)),
            },
            Panel {
                docked: self.right_sidebar_docked_width() as f32,
                // Docked, it goes along with the terminal: what of it is
                // left on screen.
                laid_out: (self.right_sidebar_width() as f32 - self.library_drawn_push() as f32)
                    .max(0.0),
                out: self.sidebar_slides.out(Side::Right, now),
            },
        )
        .map(|frame| SidebarSlideFrame { room: None, ..frame })
    }

    fn library_geometry(&self) -> Geometry {
        let border = self.get_os_border();
        let left = border.left.get() as f32;
        let top = border.top.get() as f32;
        let height = (self.dimensions.pixel_height as f32 - top - border.bottom.get() as f32).max(1.0);
        let width = self.library_shown_width();
        let rail = self.ui_f32(RAIL_WIDTH);
        let title_y = self.workspace_sidebar_content_top(top as usize) as f32;
        let cards_top = title_y + self.ui_f32(TITLE_HEIGHT) + self.ui_f32(16.0);
        let cards = euclid::rect(
            left + rail,
            cards_top,
            (width - rail).max(0.0),
            (top + height - cards_top).max(0.0),
        );
        let view = euclid::rect(left, cards.min_y(), width.max(0.0), cards.height());
        let head_y = cards_top;
        let card_y = head_y + self.ui_f32(MACHINE_HEAD) + self.ui_f32(12.0);
        let card_height = (top + height - self.ui_f32(PADDING) - card_y).max(self.ui_f32(240.0));
        Geometry {
            left,
            top,
            width,
            height,
            rail,
            title_y,
            cards,
            view,
            head_y,
            card_y,
            // A card is the sidebar, at the sidebar's width.
            card_width: self.workspace_sidebar_docked_width() as f32,
            card_height,
        }
    }

    fn library_look(&self) -> anyhow::Result<Look> {
        let chrome = self.chrome();
        let size =
            crate::native_settings::sidebar_font_size(&crate::native_settings::load_shared());
        let bold = self
            .fonts
            .title_font_with_size_and_weight(size * 0.95, 700)
            .context("library bold font")?;
        let small = self
            .fonts
            .title_font_with_size(size * 0.78)
            .context("library small font")?;
        let light = matches!(
            crate::native_settings::effective_appearance(),
            ::window::Appearance::Light | ::window::Appearance::LightHighContrast
        );
        Ok(Look {
            bold_metrics: RenderMetrics::with_font_metrics(&bold.metrics()),
            small_metrics: RenderMetrics::with_font_metrics(&small.metrics()),
            bold,
            small,
            ground: self.chrome_surface(chrome.window_bg),
            sidebar: self.sidebar_surface(chrome.workspace_sidebar_bg),
            float: chrome.control_bg,
            float_edge: chrome.control_border,
            float_hover: chrome.control_hover_bg,
            shadow: LinearRgba::with_components(0.0, 0.0, 0.0, if light { 0.30 } else { 0.60 }),
            edge_shadow: LinearRgba::with_components(0.0, 0.0, 0.0, if light { 0.16 } else { 0.45 }),
            text: chrome.text,
            muted: chrome.secondary_text,
            faint: chrome.muted_text,
            line: chrome.separator,
            accent: chrome.accent,
            button_hover: chrome.sidebar_button_hover_bg,
            danger: chrome.danger,
        })
    }

    fn refresh_library_model(&mut self, now: Instant) {
        let stale = match &self.library.model {
            None => true,
            Some(model) => {
                !self.library_moving() && now.saturating_duration_since(model.built) >= FRESH_FOR
            }
        };
        if stale {
            self.library.model = Some(self.build_library_model(now));
        }
        if !self.library_moving() {
            // Read again once it expires, even if the terminal idles: what
            // changes elsewhere -- a Space or a thread from another window, a
            // server back -- asks for no paint of this window's. Fresh, a
            // change may have asked for this one.
            if let Some(model) = &self.library.model {
                self.update_next_frame_time(Some(model.built + FRESH_FOR));
            }
        }
    }

    fn build_library_model(&self, now: Instant) -> LibraryModel {
        let spaces = workspace_threads::spaces_for_window(self.space_owner_id);
        let machines = machines(spaces)
            .into_iter()
            .map(|(domain, spaces)| {
                let (label, state) = match &domain {
                    None => (
                        crate::i18n::tr("menu-machine-local"),
                        SpaceConnectionState::Connected,
                    ),
                    Some(domain) => (
                        crate::ssh_hosts::connect_domain_label(domain),
                        self.space_connection_state(&spaces[0].id),
                    ),
                };
                Machine {
                    domain,
                    label,
                    state,
                    spaces,
                }
            })
            .collect();
        LibraryModel {
            machines,
            built: now,
        }
    }

    fn library_clip(&self, rect: RectF) -> QuadClipRect {
        QuadClipRect::from_top_left_pixels(
            rect.min_x(),
            rect.min_y(),
            rect.max_x(),
            rect.max_y(),
            &self.dimensions,
        )
    }

    fn push_library_item(&mut self, rect: RectF, within: Option<RectF>, item: LibraryItem) {
        self.push_library_ui_item(rect, within, UIItemType::Library(item));
    }

    fn push_library_ui_item(&mut self, rect: RectF, within: Option<RectF>, item_type: UIItemType) {
        let rect = match within {
            Some(within) => match rect.intersection(&within) {
                Some(rect) => rect,
                None => return,
            },
            None => rect,
        };
        if rect.width() < 1.0 || rect.height() < 1.0 {
            return;
        }
        self.ui_items.push(UIItem {
            x: rect.min_x().max(0.0) as usize,
            y: rect.min_y().max(0.0) as usize,
            width: rect.width() as usize,
            height: rect.height() as usize,
            item_type,
        });
    }

    fn library_hovered(&self, rect: RectF) -> bool {
        self.is_pointer_over_ui_rect(
            rect.min_x().max(0.0) as usize,
            rect.min_y().max(0.0) as usize,
            rect.width().max(0.0) as usize,
            rect.height().max(0.0) as usize,
        )
    }

    /// Paint the Library where the left sidebar goes, and record the
    /// sidebar on its way into its card for `paint_library_flight`.
    pub(crate) fn paint_library(&mut self, layers: &mut TripleLayerQuadAllocator) -> anyhow::Result<()> {
        let now = Instant::now();
        self.refresh_library_model(now);
        let Some(model) = self.library.model.take() else {
            return Ok(());
        };
        let painted = self.paint_library_model(layers, &model);
        self.library.model = Some(model);
        painted
    }

    fn paint_library_model(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        model: &LibraryModel,
    ) -> anyhow::Result<()> {
        let look = self.library_look()?;
        let geometry = self.library_geometry();
        let moving = self.library_moving();
        let interactive = self.library.open && !moving;
        let shift = -geometry.width * (1.0 - self.library.out.x);
        let edge_x = geometry.left + self.library_terminal_edge();
        let landed = self.library.open && !self.library.landing.moving();

        let pad = self.ui_f32(PADDING);
        let card_width = geometry.card_width;
        self.library.fresh = if moving { FRESH_MOVING } else { FRESH_STILL };
        let counts: Vec<usize> = model.machines.iter().map(|machine| machine.spaces.len()).collect();
        let (starts, row_length) = row_layout(
            &counts,
            card_width,
            self.ui_f32(CARD_GAP),
            self.ui_f32(MACHINE_GAP),
        );
        let most_scroll = (row_length + 2.0 * pad - geometry.cards.width()).max(0.0);
        if std::mem::take(&mut self.library.reveal) {
            let own = model.machines.iter().zip(&starts).find_map(|(machine, starts)| {
                machine
                    .spaces
                    .iter()
                    .position(|space| space.is_active)
                    .map(|index| starts[index])
            });
            if let Some(start) = own {
                self.library.scroll = start;
            }
        }
        self.library.scroll = self.library.scroll.clamp(0.0, most_scroll);
        // Whole pixels, so the cards' text stays sharp wherever they scroll.
        let row_x = (geometry.cards.min_x() + pad - self.library.scroll).round();

        if interactive {
            self.push_library_item(
                euclid::rect(geometry.left, 0.0, geometry.width, self.dimensions.pixel_height as f32),
                None,
                LibraryItem::Background,
            );
        }

        let mut page = HeapQuadAllocator::default();
        self.paint_library_page(&mut TripleLayerQuadAllocator::Heap(&mut page), &look, &geometry, model)?;

        let mut cards = HeapQuadAllocator::default();
        let mut slot = None;
        let mut in_view = vec![];
        for (machine, starts) in model.machines.iter().zip(&starts) {
            let (Some(first), Some(last)) = (starts.first(), starts.last()) else {
                continue;
            };
            let head = euclid::rect(
                row_x + first,
                geometry.head_y,
                last - first + card_width,
                self.ui_f32(MACHINE_HEAD),
            );
            // Only those in view: a long row has many more off it.
            if head.intersects(&geometry.view) {
                self.paint_library_machine_head(
                    &mut TripleLayerQuadAllocator::Heap(&mut cards),
                    &look,
                    machine,
                    head,
                    interactive.then_some(geometry.view),
                )?;
            }
            for (space, start) in machine.spaces.iter().zip(starts) {
                let rect = euclid::rect(
                    (row_x + start).round(),
                    geometry.card_y,
                    card_width,
                    geometry.card_height,
                );
                if space.is_active {
                    slot = Some(rect);
                    if !landed {
                        // The sidebar is on its way there.
                        continue;
                    }
                }
                if !rect.intersects(&geometry.view) {
                    continue;
                }
                self.paint_library_card(
                    &mut cards,
                    &look,
                    &geometry,
                    space,
                    rect,
                    model.built,
                    interactive,
                    moving,
                )?;
                in_view.push(space.id.clone());
            }
        }
        self.library.recordings.retain(|space, _| in_view.contains(space));
        self.apply_library_cards(&cards, &mut page, &look, &geometry, row_x, row_length)
            .context("library cards")?;
        // Over the cards running under it, in the order it is painted.
        let mut rail = HeapQuadAllocator::default();
        self.paint_library_rail(&mut TripleLayerQuadAllocator::Heap(&mut rail), &look, &geometry, interactive)?;
        rail.apply_to_single_layer(
            &mut TripleLayerQuadAllocator::Heap(&mut page),
            2,
            0.0,
            self.library_clip(euclid::rect(
                geometry.left,
                0.0,
                geometry.width,
                self.dimensions.pixel_height as f32,
            )),
        )
        .context("library rail")?;
        // The Library's edge against the terminal, over the cards' ends.
        self.filled_rectangle(
            &mut TripleLayerQuadAllocator::Heap(&mut page),
            2,
            euclid::rect(
                geometry.left + geometry.width - 1.0,
                geometry.top,
                1.0,
                geometry.height,
            ),
            look.line,
        )?;

        self.library.flight = None;
        let mut flight_shape = None;
        if let (false, Some(slot)) = (landed, slot) {
            let (mut flight, shape) = self.paint_library_flight_frame(&look, &geometry, slot, shift)?;
            flight_shape = Some(shape);
            if let Some((cut_left, cut_right)) = self.library.flight_cut {
                // Its shadow too: let go of the cut sooner and that would
                // show at once.
                let reach = 3.0 * self.ui_f32(FLIGHT_SHADOW);
                if shape.min_x() - reach >= cut_left && shape.max_x() + reach <= cut_right {
                    // Inside now, and from here on.
                    self.library.flight_cut = None;
                } else {
                    let mut cut_flight = HeapQuadAllocator::default();
                    flight
                        .apply_to_clipped(
                            &mut cut_flight,
                            QuadClipRect::from_top_left_pixels(
                                cut_left,
                                0.0,
                                cut_right,
                                self.dimensions.pixel_height as f32,
                                &self.dimensions,
                            ),
                            1.0,
                        )
                        .context("library flight out of view")?;
                    flight = cut_flight;
                    flight_shape = shape.intersection(&euclid::rect(
                        cut_left,
                        shape.min_y(),
                        cut_right - cut_left,
                        shape.height(),
                    ));
                }
            }
            self.library.flight = Some(flight);
        }
        self.paint_library_ground(
            layers,
            &look,
            &geometry,
            geometry.left + self.library_terminal_edge().round(),
            flight_shape,
        )?;

        let shown = euclid::rect(
            geometry.left,
            0.0,
            (edge_x - geometry.left).max(0.0),
            self.dimensions.pixel_height as f32,
        );
        // Going back it fades as it goes, so none of it shows past the edge
        // of the sidebar landing over it.
        let opacity = if self.library.open {
            1.0
        } else {
            clamp01(self.library.out.x / 0.5)
        };
        page.apply_to_clipped_at(layers, shift, 0.0, self.library_clip(shown), opacity)
            .context("library page")?;

        if interactive {
            let handle = self.ui_f32(EDGE_HANDLE);
            self.push_library_item(
                euclid::rect(
                    geometry.left + geometry.width - handle / 2.0,
                    geometry.top,
                    handle,
                    geometry.height,
                ),
                None,
                LibraryItem::Edge,
            );
        }
        Ok(())
    }

    /// What floats down the left: the Spaces tab -- the one kind of thing
    /// the Library holds so far -- and below, settings, which used to live
    /// in the sidebar's footer, and back, each a round button of its own.
    fn library_floats(&self, geometry: &Geometry) -> [RectF; 3] {
        let (left, rail) = (geometry.left, geometry.rail);
        let tab = euclid::rect(
            left + (rail - self.ui_f32(TAB_WIDTH)) / 2.0,
            geometry.title_y + (self.ui_f32(TITLE_HEIGHT) - self.ui_f32(TAB_HEIGHT)) / 2.0,
            self.ui_f32(TAB_WIDTH),
            self.ui_f32(TAB_HEIGHT),
        );
        let round = self.ui_f32(ROUND_BUTTON);
        let round_x = left + (rail - round) / 2.0;
        // Its foot level with the cards'.
        let back = euclid::rect(
            round_x,
            geometry.top + geometry.height - self.ui_f32(PADDING) - round,
            round,
            round,
        );
        let settings = euclid::rect(
            round_x,
            back.min_y() - self.ui_f32(ROUND_BUTTON_GAP) - round,
            round,
            round,
        );
        [tab, settings, back]
    }

    /// What the Library stands on, from the window's edge to the
    /// terminal's: where it is rather than sliding in with what stands on
    /// it, and never faded as that is going back -- nothing else is under
    /// it, and in a see-through window what nothing covers is the desktop.
    /// Under the sidebar on its way between where it docks and its card it
    /// comes in as far as the sidebar has gone, so that docked the sidebar
    /// looks as it does without the Library.
    fn paint_library_ground(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        look: &Look,
        geometry: &Geometry,
        right: f32,
        flight: Option<RectF>,
    ) -> anyhow::Result<()> {
        let height = self.dimensions.pixel_height as f32;
        let ground = euclid::rect(geometry.left, 0.0, (right - geometry.left).max(0.0), height);
        let under = flight.and_then(|shape| shape.intersection(&ground));
        let Some(under) = under else {
            self.filled_rectangle(layers, 0, ground, look.ground)?;
            return Ok(());
        };
        for part in [
            euclid::rect(ground.min_x(), 0.0, under.min_x() - ground.min_x(), height),
            euclid::rect(under.max_x(), 0.0, ground.max_x() - under.max_x(), height),
            euclid::rect(under.min_x(), 0.0, under.width(), under.min_y()),
            euclid::rect(under.min_x(), under.max_y(), under.width(), height - under.max_y()),
        ] {
            if part.width() > 0.0 && part.height() > 0.0 {
                self.filled_rectangle(layers, 0, part, look.ground)?;
            }
        }
        let along = clamp01(self.library.landing.x);
        if along > 0.0 {
            self.filled_rectangle(layers, 0, under, look.ground.mul_alpha(along))?;
        }
        Ok(())
    }

    /// The panel's title; its ground is `paint_library_ground`'s.
    fn paint_library_page(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        look: &Look,
        geometry: &Geometry,
        model: &LibraryModel,
    ) -> anyhow::Result<()> {
        let left = geometry.left;
        let rail = geometry.rail;

        // The title, and how much there is.
        let title = crate::i18n::tr("library-title");
        let title_x = left + rail + self.ui_f32(PADDING);
        let title_height = self.ui_f32(TITLE_HEIGHT);
        let bold_height = look.bold_metrics.cell_size.height as f32;
        let room = (geometry.width - rail - 2.0 * self.ui_f32(PADDING)).max(0.0);
        self.paint_sidebar_text(
            layers,
            &look.bold,
            look.bold_metrics,
            &title,
            title_x as usize,
            (geometry.title_y + (title_height - bold_height) / 2.0) as usize,
            room as usize,
            look.text,
        )?;
        let title_width = self.sidebar_text_width(&look.bold, &title)?;
        let spaces: usize = model.machines.iter().map(|machine| machine.spaces.len()).sum();
        let mut args = FluentArgs::new();
        args.set("machines", model.machines.len() as i64);
        args.set("spaces", spaces as i64);
        let count = crate::i18n::tr_args("library-count", &args);
        let count_x = title_x + title_width + self.ui_f32(18.0);
        let count_room = room - (count_x - title_x);
        if count_room > self.ui_f32(80.0) {
            self.paint_sidebar_text(
                layers,
                &look.small,
                look.small_metrics,
                &count,
                count_x as usize,
                (geometry.title_y
                    + (title_height - look.small_metrics.cell_size.height as f32) / 2.0
                    + self.ui_f32(2.0)) as usize,
                count_room as usize,
                look.muted,
            )?;
        }
        Ok(())
    }

    /// Down the left, with no ground of its own: what floats there, over
    /// the row of cards running under it -- the Spaces tab, its icon in a
    /// pill, settings and back. Names show on hover, not under them, where
    /// the cards' own words would run through them.
    fn paint_library_rail(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        look: &Look,
        geometry: &Geometry,
        interactive: bool,
    ) -> anyhow::Result<()> {
        let [tab, settings, back] = self.library_floats(geometry);
        self.paint_library_float(layers, look, tab, tab.height() / 2.0)?;
        let tab_icon = self.ui_px(40);
        self.paint_sidebar_icon(
            layers,
            SvgIcon::Layers,
            (tab.min_x() + (tab.width() - tab_icon as f32) / 2.0) as usize,
            (tab.min_y() + (tab.height() - tab_icon as f32) / 2.0) as usize,
            tab_icon,
            look.text,
        )?;

        let round = self.ui_f32(ROUND_BUTTON);
        let icon_size = self.ui_px(32);
        let icon_at = |rect: RectF| {
            (
                rect.min_x() + (rect.width() - icon_size as f32) / 2.0,
                rect.min_y() + (rect.height() - icon_size as f32) / 2.0,
            )
        };
        for (rect, icon) in [(settings, SvgIcon::Settings), (back, SvgIcon::ArrowLeft)] {
            self.paint_library_float(layers, look, rect, round / 2.0)?;
            let hovered = interactive && self.library_hovered(rect);
            if hovered {
                // Inside its hairline.
                self.fill_rounded_rectangle(
                    layers,
                    1,
                    rect.inflate(-1.0, -1.0),
                    look.float_hover,
                    round / 2.0 - 1.0,
                )?;
            }
            let (x, y) = icon_at(rect);
            self.paint_sidebar_icon(
                layers,
                icon,
                x as usize,
                y as usize,
                icon_size,
                if hovered { look.text } else { look.muted },
            )?;
        }
        // The update capsule, dismissed, flies to the dot on the shoulder of
        // settings.
        let dot = self.ui_f32(14.0);
        let (icon_x, icon_y) = icon_at(settings);
        let dot_x = icon_x + icon_size as f32 - dot * 0.75;
        let dot_y = icon_y - dot * 0.25;
        self.update_capsule.gear_dot_slot = Some((dot_x + dot / 2.0, dot_y + dot / 2.0, dot));
        if let Some(failed) = crate::termwindow::render::update_capsule::gear_dot() {
            self.fill_rounded_rectangle(
                layers,
                2,
                euclid::rect(dot_x, dot_y, dot, dot),
                if failed { look.danger } else { look.accent },
                dot / 2.0,
            )?;
        }
        if interactive {
            self.push_library_item(tab, None, LibraryItem::Spaces);
            self.push_library_item(back, None, LibraryItem::Back);
            self.ui_items.push(UIItem {
                x: settings.min_x() as usize,
                y: settings.min_y() as usize,
                width: settings.width() as usize,
                height: settings.height() as usize,
                item_type: UIItemType::WorkspaceSidebarSettings,
            });
        }
        Ok(())
    }

    /// Something floating on the Library's ground: a raised surface, as the
    /// rest of the chrome raises one, with a hairline round its edge.
    fn paint_library_float(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        look: &Look,
        rect: RectF,
        radius: f32,
    ) -> anyhow::Result<()> {
        DrawContext::new(
            self.render_state.as_ref().unwrap(),
            self.dimensions,
            &self.render_metrics,
        )
        .draw_elevated_surface(layers, 0, rect, look.float, look.float_edge, look.shadow, radius)
    }

    /// A machine's name over its cards, how it stands, and its +.
    fn paint_library_machine_head(
        &mut self,
        outer: &mut TripleLayerQuadAllocator,
        look: &Look,
        machine: &Machine,
        head: RectF,
        within: Option<RectF>,
    ) -> anyhow::Result<()> {
        // What paints text and icons takes no place left of the window: a
        // head running past its edge is painted further in, and moved back.
        let lift = (-head.min_x()).max(0.0).ceil();
        let real = head;
        let head = head.translate(euclid::vec2(lift, 0.0));
        let mut quads = HeapQuadAllocator::default();
        let layers = &mut TripleLayerQuadAllocator::Heap(&mut quads);
        let small_height = look.small_metrics.cell_size.height as f32;
        let text_y = (head.min_y() + (head.height() - small_height) / 2.0) as usize;
        let mut x = head.min_x() + self.ui_f32(4.0);
        let connected = machine.state == SpaceConnectionState::Connected;
        match &machine.domain {
            None => {
                let size = self.ui_px(28);
                self.paint_sidebar_icon(
                    layers,
                    SvgIcon::Laptop,
                    x as usize,
                    (head.min_y() + (head.height() - size as f32) / 2.0) as usize,
                    size,
                    look.muted,
                )?;
                x += size as f32 + self.ui_f32(14.0);
            }
            Some(_) => {
                let dot = self.ui_f32(16.0);
                let color = match machine.state {
                    SpaceConnectionState::Connected => crate::thread_marks::FINISHED_COLOR,
                    SpaceConnectionState::Connecting | SpaceConnectionState::Reconnecting => {
                        crate::thread_marks::WAITING_COLOR
                    }
                    SpaceConnectionState::Disconnected => look.faint,
                };
                self.fill_rounded_rectangle(
                    layers,
                    1,
                    euclid::rect(x, head.min_y() + (head.height() - dot) / 2.0, dot, dot),
                    color,
                    dot / 2.0,
                )?;
                x += dot + self.ui_f32(14.0);
            }
        }
        let plus = self.ui_f32(48.0);
        let room = (head.max_x() - plus - self.ui_f32(12.0) - x).max(0.0);
        self.paint_sidebar_text(layers, &look.small, look.small_metrics, &machine.label, x as usize, text_y, room as usize, look.muted)?;
        if !connected {
            let state = match machine.state {
                SpaceConnectionState::Disconnected => crate::i18n::tr("library-disconnected"),
                _ => crate::i18n::tr("sidebar-connecting"),
            };
            let label_width = self.sidebar_text_width(&look.small, &machine.label)?;
            let state_x = x + label_width + self.ui_f32(12.0);
            if state_x < head.max_x() - plus {
                self.paint_sidebar_text(
                    layers,
                    &look.small,
                    look.small_metrics,
                    &state,
                    state_x as usize,
                    text_y,
                    (head.max_x() - plus - state_x).max(0.0) as usize,
                    look.faint,
                )?;
            }
        } else {
            let rect = euclid::rect(head.max_x() - plus, head.min_y(), plus, head.height());
            let at = euclid::rect(real.max_x() - plus, real.min_y(), plus, real.height());
            let hovered = within.is_some() && self.library_hovered(at);
            if hovered {
                self.fill_rounded_rectangle(layers, 1, rect, look.button_hover, self.ui_f32(14.0))?;
            }
            let size = self.ui_px(30);
            self.paint_sidebar_icon(
                layers,
                SvgIcon::Plus,
                (rect.min_x() + (rect.width() - size as f32) / 2.0) as usize,
                (rect.min_y() + (rect.height() - size as f32) / 2.0) as usize,
                size,
                if hovered { look.text } else { look.muted },
            )?;
            if let Some(within) = within {
                self.push_library_item(at, Some(within), LibraryItem::NewSpace(machine.domain.clone()));
            }
        }
        quads.apply_to_at(outer, -lift, 0.0)
    }

    /// Where a card's sidebar is laid out: its Space row `CARD_INSET` below
    /// the card's top, and its footer, which the card goes without, from
    /// `CARD_INSET` above the card's bottom -- where its list ends.
    fn library_card_panel(&self, card: RectF) -> CardPanel {
        let inset = self.ui_px(CARD_INSET) as f32;
        let top = (card.min_y() + inset - self.workspace_sidebar_space_row_offset() as f32).round();
        let footer = self.ui_px(SIDEBAR_SETTINGS_FOOTER_HEIGHT) as f32;
        let bottom = (card.max_y() - inset + footer).round();
        CardPanel {
            x: card.min_x().round(),
            y: top,
            width: card.width().round().max(1.0) as usize,
            height: (bottom - top).max(1.0) as usize,
        }
    }

    /// How far `space`'s sidebar is scrolled: where this window has it, or
    /// where it was left.
    fn library_card_scroll(&self, space: &str) -> f32 {
        if space == self.active_space_id {
            self.workspace_sidebar_scroll_offset
        } else {
            self.workspace_sidebar_scroll_offsets
                .get(space)
                .copied()
                .unwrap_or(0.0)
        }
    }

    /// One Space's card: its sidebar, as the sidebar paints it, on the
    /// sidebar's colour with rounded corners -- the window's own outlined in
    /// its accent.
    #[allow(clippy::too_many_arguments)]
    fn paint_library_card(
        &mut self,
        cards: &mut HeapQuadAllocator,
        look: &Look,
        geometry: &Geometry,
        space: &SpaceView,
        rect: RectF,
        built: Instant,
        interactive: bool,
        moving: bool,
    ) -> anyhow::Result<()> {
        let Some(visible) = rect.intersection(&geometry.view) else {
            return Ok(());
        };
        let panel = self.library_card_panel(rect);
        let pointer = self
            .current_mouse_event
            .as_ref()
            .filter(|_| interactive)
            .map(|event| event.coords)
            .filter(|coords| {
                let at = euclid::point2(coords.x as f32, coords.y as f32);
                // Under what floats on the rail, it is over that instead.
                visible.contains(at) && !self.library_floats(geometry).iter().any(|float| float.contains(at))
            })
            .map(|coords| (coords.x - panel.x as isize, coords.y - panel.y as isize));
        let key = RecordingKey {
            built,
            width: panel.width,
            height: panel.height,
            scroll: self.library_card_scroll(&space.id),
            pointer,
        };
        let now = Instant::now();
        let recording = match self.library.recordings.remove(&space.id) {
            // What moves on a card waits while the Library itself moves.
            Some(recording)
                if recording.key == key
                    && (moving || recording.sidebar.due.map_or(true, |due| due > now)) =>
            {
                // As it was: what moves on it still wants waking for.
                self.update_next_frame_time(recording.sidebar.due);
                Some(recording)
            }
            _ if self.library.fresh > 0 => {
                self.library.fresh -= 1;
                let (at, _) = panel.recorded_at();
                let sidebar = self.record_library_sidebar(
                    &space.id,
                    panel.rect_at(at),
                    key.scroll,
                    pointer.map(|(x, y)| (x + at.0 as isize, y + at.1 as isize)),
                )?;
                Some(Recording { key, at, sidebar })
            }
            stale => {
                // Painted afresh next frame; until then as it was, where it
                // still fits, or empty.
                if let Some(window) = self.window.as_ref() {
                    window.invalidate();
                }
                stale.filter(|stale| stale.key.width == key.width && stale.key.height == key.height)
            }
        };

        let own = space.is_active;
        let radius = self.ui_f32(CARD_RADIUS);
        let mut card = HeapQuadAllocator::default();
        self.fill_rounded_rectangle(&mut TripleLayerQuadAllocator::Heap(&mut card), 0, rect, look.sidebar, radius)?;
        if let Some(recording) = &recording {
            recording
                .sidebar
                .quads
                .apply_to_clipped_at(
                    &mut card,
                    panel.x - recording.at.0,
                    panel.y - recording.at.1,
                    self.library_clip(rect),
                    1.0,
                )
                .context("library card")?;
        }
        self.paint_library_card_outline(&mut TripleLayerQuadAllocator::Heap(&mut card), look, rect, own, radius, 1.0)?;
        card.apply_to_clipped(cards, self.library_clip(visible), 1.0)
            .context("library card in view")?;

        if let Some(recording) = recording {
            if interactive {
                let (dx, dy) = (panel.x - recording.at.0, panel.y - recording.at.1);
                self.push_library_card_items(&recording.sidebar.items, space, dx, dy, visible);
            }
            self.library.recordings.insert(space.id.clone(), recording);
        }
        Ok(())
    }

    /// A card's edge: a hairline, or two pixels of accent on the window's own.
    /// Outlined, not laid under the card: a card is see-through in a
    /// see-through window, and a rectangle of the edge's colour under it would
    /// show through.
    fn paint_library_card_outline(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        look: &Look,
        rect: RectF,
        own: bool,
        radius: f32,
        opacity: f32,
    ) -> anyhow::Result<()> {
        let border = if own { look.accent } else { look.line }.mul_alpha(opacity);
        let draw = DrawContext::new(
            self.render_state.as_ref().unwrap(),
            self.dimensions,
            &self.render_metrics,
        );
        if !own {
            return draw.draw_rounded_frame(
                layers,
                2,
                rect.min_x(),
                rect.min_y(),
                rect.width(),
                rect.height(),
                LinearRgba::with_components(0.0, 0.0, 0.0, 0.0),
                border,
                radius,
            );
        }
        // Two pixels: the straight runs filled, the corners stamped along
        // their arc. Not two one-pixel rings, one inside the other: where
        // their soft edges meet they come to less than solid, and round the
        // corners that read as a dotted line.
        let stroke = 2.0;
        let (x, y) = (rect.min_x().round(), rect.min_y().round());
        let (width, height) = (rect.width().round(), rect.height().round());
        let radius = radius.round().min(width / 2.0).min(height / 2.0).max(stroke);
        let run_x = (width - 2.0 * radius).max(0.0);
        let run_y = (height - 2.0 * radius).max(0.0);
        draw.draw_rect(layers, 2, x + radius, y, run_x, stroke, border)?;
        draw.draw_rect(layers, 2, x + radius, y + height - stroke, run_x, stroke, border)?;
        draw.draw_rect(layers, 2, x, y + radius, stroke, run_y, border)?;
        draw.draw_rect(layers, 2, x + width - stroke, y + radius, stroke, run_y, border)?;
        use std::f32::consts::{FRAC_PI_2, PI};
        let along = radius - stroke / 2.0;
        // A stamp every half pixel of arc.
        let stamps = (along * FRAC_PI_2 / 0.5).ceil().max(1.0) as usize;
        for (cx, cy, from) in [
            (x + radius, y + radius, PI),
            (x + width - radius, y + radius, PI + FRAC_PI_2),
            (x + width - radius, y + height - radius, 0.0),
            (x + radius, y + height - radius, FRAC_PI_2),
        ] {
            for stamp in 0..=stamps {
                let angle = from + FRAC_PI_2 * stamp as f32 / stamps as f32;
                draw.draw_disc(layers, 2, cx + along * angle.cos(), cy + along * angle.sin(), stroke, border)?;
            }
        }
        Ok(())
    }

    /// What a card's sidebar registered for the pointer, as the Library
    /// takes it: a thread opens that thread, the Space row goes to the
    /// Space and its ⋯ opens the Space's menu, the rest of another Space's
    /// card goes there. Moved as its card's sidebar was, and cut to what is
    /// in view of it.
    fn push_library_card_items(&mut self, items: &[UIItem], space: &SpaceView, dx: f32, dy: f32, within: RectF) {
        let own = space.is_active;
        // Another window has it: only its menu.
        let open = !space.is_occupied_by_other_window;
        let id = || space.id.clone();
        for item in items {
            let rect = euclid::rect(
                item.x as f32 + dx,
                item.y as f32 + dy,
                item.width as f32,
                item.height as f32,
            );
            let within = Some(within);
            match &item.item_type {
                UIItemType::WorkspaceThread(thread) if open => self.push_library_item(
                    rect,
                    within,
                    LibraryItem::Thread {
                        space: id(),
                        thread: thread.clone(),
                    },
                ),
                UIItemType::SpaceMenu if own || !open => {
                    self.push_library_item(rect, within, LibraryItem::SpaceMenu(id()))
                }
                UIItemType::SpaceMenu => {
                    self.push_library_item(rect, within, LibraryItem::Space(id()));
                    let more = rect.height();
                    self.push_library_item(
                        euclid::rect(rect.max_x() - more, rect.min_y(), more, more),
                        within,
                        LibraryItem::SpaceMenu(id()),
                    );
                }
                UIItemType::SpaceReconnect => {
                    self.push_library_item(rect, within, LibraryItem::Connect(id()))
                }
                // The window's notifications, from any card's bell; a new
                // thread from the window's own card.
                UIItemType::WorkspaceSidebarNotifications => {
                    self.push_library_ui_item(rect, within, item.item_type.clone())
                }
                UIItemType::WorkspaceThreadNew(_) if own => {
                    self.push_library_ui_item(rect, within, item.item_type.clone())
                }
                // Not the panel's own edge, header or footer -- whose row
                // reaches up over the list's last -- nor the buttons a row
                // shows under the pointer: a click there is the row's.
                UIItemType::WorkspaceSidebarResize
                | UIItemType::WorkspaceSidebarHeaderBlank
                | UIItemType::WorkspaceSidebarToggle
                | UIItemType::Library(LibraryItem::Open)
                | UIItemType::WorkspaceSidebarViewOptions
                | UIItemType::WorkspaceSidebarThreadSearch
                | UIItemType::WorkspaceSidebarSshHosts
                | UIItemType::WorkspaceSidebarLiveOverview
                | UIItemType::WorkspaceSidebarSettings
                | UIItemType::WorkspaceThreadPin(_)
                | UIItemType::WorkspaceThreadDelete(_) => {}
                // Each of the rest its own item, so what the pointer is over
                // is painted again as it moves between them.
                _ if !own && open => self.push_library_item(rect, within, LibraryItem::Space(id())),
                _ => self.push_library_item(rect, within, LibraryItem::Background),
            }
        }
    }

    /// Lay the row of cards on the page. Where it runs under the Library's
    /// edge the terminal casts a narrow shadow over it rather than the cards
    /// stopping dead there; black, so it reads over any ground, a see-through
    /// one included. Scrolled, it runs on under the rail to the panel's own
    /// edge.
    fn apply_library_cards(
        &self,
        cards: &HeapQuadAllocator,
        page: &mut HeapQuadAllocator,
        look: &Look,
        geometry: &Geometry,
        row_x: f32,
        row_length: f32,
    ) -> anyhow::Result<()> {
        let area = geometry.cards;
        cards.apply_to_clipped(page, self.library_clip(geometry.view), 1.0)?;
        let width = self.ui_f32(EDGE_SHADOW).min(area.width() / 3.0).floor();
        if width < 1.0 {
            return Ok(());
        }
        let pad = self.ui_f32(PADDING);
        let right = overflow_strength(row_x + row_length - (area.max_x() - pad), width);
        let mut layers = TripleLayerQuadAllocator::Heap(page);
        let steps = width as usize;
        // The panel's full height, as its edge line: cut off where the row
        // of cards begins, the shadow would end in a step.
        let column = |x: f32| euclid::rect(x, geometry.top, 1.0, geometry.height);
        for step in 0..steps {
            // Nothing at its inner end, deepening towards the edge.
            let t = (step as f32 + 0.5) / steps as f32;
            if right > 0.0 {
                self.filled_rectangle(
                    &mut layers,
                    2,
                    column(area.max_x() - width + step as f32),
                    look.edge_shadow.mul_alpha(t * t * right),
                )?;
            }
        }
        Ok(())
    }

    /// `space`'s sidebar as `paint_workspace_sidebar` paints it, laid out in
    /// `rect` with its list scrolled by `scroll`. What it registers for the
    /// pointer comes back with it rather than being kept, and so does when
    /// anything on it moves next. `pointer`, in the same pixels, is where
    /// the pointer is for what it is over to show so; `None` shows nothing
    /// so.
    fn record_library_sidebar(
        &mut self,
        space: &str,
        rect: WorkspaceSidebarRect,
        scroll: f32,
        pointer: Option<(isize, isize)>,
    ) -> anyhow::Result<SidebarRecording> {
        let items = self.ui_items.len();
        let images = self.image_composites.borrow().mark();
        // The sidebar paint reads all of these as the window's own sidebar
        // has them; each goes back as it was.
        let live_scroll = std::mem::replace(&mut self.workspace_sidebar_scroll_offset, scroll);
        let preview = self
            .workspace_sidebar_preview_space_id
            .replace(space.to_string());
        let collapsed = std::mem::replace(&mut self.workspace_sidebar_collapsed, false);
        let scrollbar = self.workspace_sidebar_scrollbar_visible_until.take();
        let gear_dot = self.update_capsule.gear_dot_slot;
        let mouse = self.current_mouse_event.clone();
        self.current_mouse_event = pointer.and_then(|(x, y)| {
            mouse.clone().map(|mut event| {
                event.coords = window::Point::new(x, y);
                event
            })
        });
        let animation = self.has_animation.borrow_mut().take();
        self.library.card = Some(rect);
        self.library.card_footer = None;
        self.library.card_toolbar = None;

        let mut quads = HeapQuadAllocator::default();
        let painted = self.paint_workspace_sidebar(&mut TripleLayerQuadAllocator::Heap(&mut quads));

        self.library.card = None;
        let footer = self.library.card_footer.take();
        let toolbar = self.library.card_toolbar.take();
        let due = self.has_animation.borrow_mut().take();
        self.update_next_frame_time(animation);
        self.update_next_frame_time(due);
        self.current_mouse_event = mouse;
        self.update_capsule.gear_dot_slot = gear_dot;
        self.workspace_sidebar_scrollbar_visible_until = scrollbar;
        self.workspace_sidebar_collapsed = collapsed;
        self.workspace_sidebar_preview_space_id = preview;
        self.workspace_sidebar_scroll_offset = live_scroll;
        // Nothing of it is where the window's sidebar is: no images, no
        // items, no marks into a recording about to be moved.
        drop(self.image_composites.borrow_mut().take_since(images));
        let recorded = self.ui_items.split_off(items);
        self.workspace_sidebar_list_quads = None;
        painted?;

        // Body, toolbar and footer: the footer is the end of it, the toolbar
        // a stretch before the body goes on.
        let whole = QuadClipRect::from_top_left_pixels(-1.0e6, -1.0e6, 1.0e6, 1.0e6, &self.dimensions);
        let end = quads.mark();
        let footer_at = footer.unwrap_or(end);
        let mut body = HeapQuadAllocator::default();
        let mut toolbar_quads = HeapQuadAllocator::default();
        match toolbar {
            Some((start, stop)) => {
                quads.apply_before(&mut body, &start)?;
                quads.apply_between(&mut toolbar_quads, &start, &stop, 0.0, whole)?;
                quads.apply_between(&mut body, &stop, &footer_at, 0.0, whole)?;
            }
            None => quads.apply_before(&mut body, &footer_at)?,
        }
        let mut footer_quads = HeapQuadAllocator::default();
        quads.apply_after(&mut footer_quads, &footer_at)?;
        Ok(SidebarRecording {
            quads: body,
            footer: footer_quads,
            toolbar: toolbar_quads,
            items: recorded,
            due,
        })
    }

    /// The sidebar this frame on its way into its card, or out of it: one
    /// curve out over the terminal and round, its corners rounding and its
    /// list closing up to the card's as it goes, its footer left behind.
    fn paint_library_flight_frame(
        &mut self,
        look: &Look,
        geometry: &Geometry,
        slot: RectF,
        shift: f32,
    ) -> anyhow::Result<(HeapQuadAllocator, RectF)> {
        let along = self.library.landing.x;
        let k = clamp01(along);
        let width = geometry.card_width;
        let docked_x = geometry.left;
        // Folded away, it comes out from behind the window's edge -- or from
        // as far out as the pointer had brought it.
        let start_x = docked_x - width * (1.0 - self.library.sidebar_out);
        let card_x = slot.min_x() + shift;
        let swing = (slot.min_x() + self.ui_f32(SWING)).max(docked_x + self.ui_f32(SWING_LEAST));
        let x = swing_x(start_x, swing, card_x, along);
        let bottom = geometry.top + geometry.height;
        let shape_top = lerp(geometry.top, slot.min_y(), k);
        let shape = euclid::rect(
            x,
            shape_top,
            width,
            lerp(bottom, slot.max_y(), k) - shape_top,
        );
        // Laid out between where the sidebar docks and where its card lays
        // it out, so its header rides up and its list closes up as it goes.
        let card = self.library_card_panel(slot);
        let top = lerp(geometry.top, card.y, k);
        let panel = CardPanel {
            x: docked_x,
            y: top,
            width: width as usize,
            height: (lerp(bottom, card.y + card.height as f32, k) - top).round().max(1.0) as usize,
        };
        let (at, (_, dy)) = panel.recorded_at();
        let space = self.active_space_id.clone();
        let scroll = self.library_card_scroll(&space);
        let sidebar = self.record_library_sidebar(&space, panel.rect_at(at), scroll, None)?;
        let dx = x - at.0;
        let radius = self.ui_f32(CARD_RADIUS) * k;
        let lift = (std::f32::consts::PI * k).sin();

        let mut flight = HeapQuadAllocator::default();
        {
            let mut layers = TripleLayerQuadAllocator::Heap(&mut flight);
            if lift > 0.01 {
                let peak = match crate::native_settings::effective_appearance() {
                    ::window::Appearance::Light | ::window::Appearance::LightHighContrast => 0.18,
                    ::window::Appearance::Dark | ::window::Appearance::DarkHighContrast => 0.45,
                };
                // At the card's corners whatever the shape's are this frame:
                // a shadow's sprite is made once for each corner and blur,
                // and one made a frame is a stall a frame. Under that much
                // blur the difference does not show.
                DrawContext::new(self.render_state.as_ref().unwrap(), self.dimensions, &self.render_metrics)
                    .draw_shadow(
                        &mut layers,
                        0,
                        shape,
                        self.ui_f32(CARD_RADIUS),
                        self.ui_f32(FLIGHT_SHADOW),
                        self.ui_f32(10.0),
                        LinearRgba::with_components(0.0, 0.0, 0.0, peak * lift),
                    )?;
            }
            self.fill_rounded_rectangle(&mut layers, 0, shape, look.sidebar, radius)?;
        }
        let clip = self.library_clip(shape);
        sidebar
            .quads
            .apply_to_clipped_at(&mut flight, dx, dy, clip, 1.0)
            .context("library flight sidebar")?;
        // Its footer and toolbar stay behind as it lifts out.
        let left_behind = 1.0 - clamp01(along / 0.3);
        if left_behind > 0.0 {
            for part in [&sidebar.footer, &sidebar.toolbar] {
                part.apply_to_clipped_at(&mut flight, dx, dy, clip, left_behind)
                    .context("library flight footer")?;
            }
        }
        // The card's edge comes in as it lands.
        let outline = clamp01((along - 0.5) / 0.5);
        if outline > 0.0 {
            self.paint_library_card_outline(
                &mut TripleLayerQuadAllocator::Heap(&mut flight),
                look,
                shape,
                true,
                radius,
                outline,
            )?;
        }
        Ok((flight, shape))
    }

    /// Draw the sidebar on its way, over everything else at its depth --
    /// the terminal's glyphs and the tab row included.
    pub(crate) fn paint_library_flight(&mut self) -> anyhow::Result<()> {
        let Some(flight) = self.library.flight.take() else {
            return Ok(());
        };
        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state
            .layer_for_zindex(0)
            .context("library flight layer")?;
        let mut layers = layer.quad_allocator();
        let clip = QuadClipRect::from_top_left_pixels(
            0.0,
            0.0,
            self.dimensions.pixel_width as f32,
            self.dimensions.pixel_height as f32,
            &self.dimensions,
        );
        flight.apply_to_single_layer(&mut layers, 2, 0.0, clip)
    }

    pub(crate) fn mouse_event_library(
        &mut self,
        item: UIItem,
        which: LibraryItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        match which {
            LibraryItem::Background | LibraryItem::Spaces => context.set_cursor(Some(MouseCursor::Arrow)),
            LibraryItem::Edge => {
                context.set_cursor(Some(MouseCursor::SizeLeftRight));
                if event.kind == WMEK::Press(MousePress::Left) && self.library.open {
                    let width = self.library_width();
                    self.library.edge = Some(Edge {
                        grab: event.coords.x,
                        from: width,
                        width,
                        back: None,
                        spring: Spring::default(),
                    });
                    self.dragging.replace((item, event));
                }
            }
            which => {
                context.set_cursor(Some(MouseCursor::Hand));
                match event.kind {
                    WMEK::Press(MousePress::Left) => {
                        let below = window::Point::new(
                            item.x as isize,
                            item.y.saturating_add(item.height) as isize,
                        );
                        self.activate_library_item(which, below, context);
                    }
                    WMEK::Press(MousePress::Right) => {
                        if let LibraryItem::Space(space) | LibraryItem::SpaceMenu(space) = &which {
                            self.show_library_space_menu(space, event.coords, context);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn activate_library_item(&mut self, which: LibraryItem, below: window::Point, context: &dyn WindowOps) {
        match which {
            LibraryItem::Open => self.open_library(context),
            LibraryItem::Back => self.close_library(context),
            LibraryItem::Space(space) => {
                self.go_to_library_space(space, None, context)
            }
            LibraryItem::Connect(space) if space == self.active_space_id => {
                self.reconnect_space_domain();
                self.library.model = None;
            }
            LibraryItem::Connect(space) => self.go_to_library_space(space, None, context),
            LibraryItem::Thread { space, thread } => {
                self.go_to_library_space(space, Some(thread), context)
            }
            LibraryItem::NewSpace(domain) => {
                // A card beside the rest: the window stays where it is, and
                // so does the Space a window opens on next.
                match domain {
                    None => {
                        workspace_threads::add_space(None);
                    }
                    Some(domain) => {
                        if let Err(err) = workspace_threads::add_space_on_domain(&domain, None) {
                            log::warn!("failed to create a Space on {domain}: {err:#}");
                        }
                    }
                }
                self.library.model = None;
            }
            LibraryItem::SpaceMenu(space) => self.show_library_space_menu(&space, below, context),
            LibraryItem::Background | LibraryItem::Spaces | LibraryItem::Edge => {}
        }
        context.invalidate();
    }

    /// Show `space`, and `thread` in it if one is named, in this window.
    /// A server's Space that is not connected connects on the way, the way
    /// the Space menu's does. Another Space chosen, the Library has done
    /// what it was opened for and goes back, the chosen card becoming the
    /// sidebar.
    fn go_to_library_space(&mut self, space: String, thread: Option<String>, context: &dyn WindowOps) {
        let elsewhere = space != self.active_space_id;
        // Taken from the row as it stands, before the switch rebuilds it.
        let reveal = elsewhere.then(|| self.library_glide_to(&space)).flatten();
        let Some(window) = self.window.clone() else {
            return;
        };
        if !self.switch_space_to_thread(space, thread, &window) {
            return;
        }
        self.library.model = None;
        if !elsewhere {
            return;
        }
        self.close_library(context);
        // Its card partly out of view: as it goes back the row brings it in
        // -- one motion, not a glide and then a flight. The chosen card,
        // even before a server's Space is switched to.
        if let Some((glide, cut)) = reveal {
            self.library.glide = Some(glide);
            self.library.flight_cut = Some(cut);
        }
    }

    /// The glide bringing `space`'s card from where it is to the middle of
    /// the row's view, if it is not wholly in view, and where the row is in
    /// view, left and right, for its flight back to be cut to.
    fn library_glide_to(&self, space: &str) -> Option<(Glide, (f32, f32))> {
        let model = self.library.model.as_ref()?;
        let geometry = self.library_geometry();
        let pad = self.ui_f32(PADDING);
        let card = geometry.card_width;
        let counts: Vec<usize> = model.machines.iter().map(|machine| machine.spaces.len()).collect();
        let (starts, length) = row_layout(&counts, card, self.ui_f32(CARD_GAP), self.ui_f32(MACHINE_GAP));
        let start = model.machines.iter().zip(&starts).find_map(|(machine, starts)| {
            machine
                .spaces
                .iter()
                .position(|candidate| candidate.id == space)
                .map(|index| starts[index])
        })?;
        let view = geometry.cards;
        let left = view.min_x() + pad - self.library.scroll + start;
        if left >= view.min_x() && left + card <= view.max_x() {
            return None;
        }
        let most = (length + 2.0 * pad - view.width()).max(0.0);
        let to = (pad + start + card / 2.0 - view.width() / 2.0).clamp(0.0, most);
        let mut spring = Spring::default();
        spring.to(1.0, GLIDE, 0.0);
        Some((
            Glide {
                from: self.library.scroll,
                to,
                spring,
            },
            (geometry.view.min_x(), view.max_x()),
        ))
    }

    fn show_library_space_menu(&mut self, space_id: &str, coords: window::Point, context: &dyn WindowOps) {
        let Some(space) = workspace_threads::spaces_for_window(self.space_owner_id)
            .into_iter()
            .find(|space| space.id == space_id)
        else {
            return;
        };
        // Another window has it, and this one cannot go to it to rename it
        // in its sidebar: say so, as the Space menu does.
        if space.is_occupied_by_other_window {
            let occupied = ContextMenuItem::item_with_icon(
                with_name("menu-space-occupied", &space.name),
                ContextMenuIcon::Stack,
                KeyAssignment::SwitchSpace(space.id.clone()),
            )
            .disabled();
            self.show_term_context_menu(context, coords, vec![occupied]);
            return;
        }
        let mut items = vec![ContextMenuItem::item_with_icon(
            with_name("menu-rename-named-space", &space.name),
            ContextMenuIcon::Edit,
            KeyAssignment::PromptRenameSpace(space.id.clone()),
        )];
        if !space.is_default && !space.is_occupied_by_other_window {
            items.push(ContextMenuItem::Separator);
            items.extend(crate::termwindow::mouseevent::space_destructive_menu_items(
                &space,
                space.is_domain_attached,
            ));
        }
        self.show_term_context_menu(context, coords, items);
    }

    /// A wheel or a trackpad over the Library scrolls its row of cards.
    /// `None` when the pointer is not over it, or this is not a wheel: the
    /// wheel handlers are asked of every mouse event, and taking a press here
    /// would keep it from everything the Library has to click.
    pub(crate) fn scroll_library(
        &mut self,
        event: &MouseEvent,
        sideways: Option<f32>,
        down: Option<f32>,
        context: &dyn WindowOps,
    ) -> Option<bool> {
        if !self.library_presented() || !matches!(event.kind, WMEK::VertWheel(_) | WMEK::HorzWheel(_)) {
            return None;
        }
        // What a swipe that took the Library in or out, or might have,
        // flings on with is not for the cards -- until the next one begins.
        if matches!(event.scroll_phase, Some(ScrollPhase::Began)) {
            self.library.swipe_momentum = false;
        }
        if let Some(momentum) = event.momentum_phase {
            let ended = matches!(momentum, ScrollPhase::Ended | ScrollPhase::Cancelled);
            let after_space_swipe = self.workspace_sidebar_swipe.consume_momentum(ended);
            let after_own = self.library.swipe_momentum;
            if ended {
                self.library.swipe_momentum = false;
            }
            if after_space_swipe || after_own {
                return Some(true);
            }
        }
        if let Some(handled) = self.swipe_library_back(event, context) {
            return Some(handled);
        }
        let left = self.get_os_border().left.get() as f32;
        let x = event.coords.x as f32;
        if x < left || x >= left + self.library_terminal_edge() {
            return None;
        }
        if self.library_moving() {
            return Some(true);
        }
        // Up and down over a card whose list runs past it scrolls that list;
        // anything else goes along the row.
        if let (WMEK::VertWheel(_), Some(delta)) = (&event.kind, down) {
            let at = euclid::point2(x, event.coords.y as f32);
            if let Some((space, card)) = self.library_card_at(at) {
                let most = self.library_card_scroll_most(&space, card);
                if most > 0.0 {
                    let scroll = (self.library_card_scroll(&space) + delta).clamp(0.0, most);
                    if space == self.active_space_id {
                        self.workspace_sidebar_scroll_offset = scroll;
                    } else {
                        self.workspace_sidebar_scroll_offsets.insert(space, scroll);
                    }
                    context.invalidate();
                    return Some(true);
                }
            }
        }
        if let Some(delta) = sideways.or(down) {
            self.library.scroll += delta;
            context.invalidate();
        }
        Some(true)
    }

    /// A trackpad swipe begun on the Library beside its cards -- the rail,
    /// the title, between and under them: sideways towards the terminal it
    /// takes the Library back with it, as far as the finger goes. `None`
    /// for any other, the cards' to scroll.
    fn swipe_library_back(&mut self, event: &MouseEvent, context: &dyn WindowOps) -> Option<bool> {
        let phase = library_swipe_phase(event.scroll_phase, event.momentum_phase)?;
        let now = Instant::now();
        let (dx, dy) = event
            .precise_scroll_delta
            .map(|delta| (delta.x, delta.y))
            .unwrap_or((0.0, 0.0));
        if matches!(phase, ScrollPhase::Began) {
            self.library.swipe = None;
            let left = self.get_os_border().left.get() as f32;
            let at = euclid::point2(event.coords.x as f32, event.coords.y as f32);
            if self.library.open
                && !self.library_moving()
                && at.x >= left
                && at.x < left + self.library_terminal_edge()
                && self.library_card_at(at).is_none()
            {
                self.library.swipe = Some(Swipe {
                    x: 0.0,
                    y: 0.0,
                    velocity: 0.0,
                    last: now,
                    sideways: false,
                });
            }
        }
        let mut swipe = self.library.swipe?;
        if matches!(phase, ScrollPhase::Began | ScrollPhase::Changed) {
            let dt = now
                .saturating_duration_since(swipe.last)
                .as_secs_f32()
                .clamp(1.0 / 240.0, 1.0 / 20.0);
            swipe.last = now;
            swipe.x += dx;
            swipe.y += dy;
            // Towards the terminal is the way the next Space is swiped to.
            swipe.velocity = swipe.velocity * 0.65 + (-dx / dt) * 0.35;
            if !swipe.sideways {
                let (x, y) = (swipe.x.abs(), swipe.y.abs());
                if x >= 8.0 && x > y * 1.15 && swipe.x > 0.0 {
                    // Away from the terminal: the cards' to scroll along.
                    self.library.swipe = None;
                    return None;
                } else if x >= 8.0 && x > y * 1.15 {
                    swipe.sideways = true;
                } else if y >= 18.0 {
                    // Up or down: the cards' as any wheel is.
                    self.library.swipe = None;
                    return None;
                }
            }
        }
        self.library.swipe = Some(swipe);
        if swipe.sideways {
            self.hold_library(false, (-swipe.x).max(0.0), context);
        }
        if matches!(phase, ScrollPhase::Ended | ScrollPhase::Cancelled) {
            self.library.swipe = None;
            if swipe.sideways {
                self.library.swipe_momentum = true;
                self.let_go_library(swipe.velocity, context);
            }
        }
        Some(true)
    }

    /// The card at `at`, if one is, not under what floats on the rail: its
    /// Space, and where it is.
    fn library_card_at(&self, at: window::PointF) -> Option<(String, RectF)> {
        let model = self.library.model.as_ref()?;
        let geometry = self.library_geometry();
        if !geometry.view.contains(at) || self.library_floats(&geometry).iter().any(|float| float.contains(at)) {
            return None;
        }
        let card = geometry.card_width;
        let counts: Vec<usize> = model.machines.iter().map(|machine| machine.spaces.len()).collect();
        let (starts, _) = row_layout(&counts, card, self.ui_f32(CARD_GAP), self.ui_f32(MACHINE_GAP));
        let row_x = (geometry.cards.min_x() + self.ui_f32(PADDING) - self.library.scroll).round();
        model.machines.iter().zip(&starts).find_map(|(machine, starts)| {
            machine.spaces.iter().zip(starts).find_map(|(space, start)| {
                let rect = euclid::rect((row_x + start).round(), geometry.card_y, card, geometry.card_height);
                rect.contains(at).then(|| (space.id.clone(), rect))
            })
        })
    }

    /// How far `space`'s card, laid out at `card`, can scroll its list: as
    /// far as its sidebar could, laid out there.
    fn library_card_scroll_most(&mut self, space: &str, card: RectF) -> f32 {
        let panel = self.library_card_panel(card);
        let (at, _) = panel.recorded_at();
        let preview = self
            .workspace_sidebar_preview_space_id
            .replace(space.to_string());
        let collapsed = std::mem::replace(&mut self.workspace_sidebar_collapsed, false);
        let laid_out = self.library.card.replace(panel.rect_at(at));
        let most = self.workspace_sidebar_scroll_max();
        self.library.card = laid_out;
        self.workspace_sidebar_collapsed = collapsed;
        self.workspace_sidebar_preview_space_id = preview;
        most
    }

    pub(crate) fn drag_library_edge(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let (least, most) = self.library_edge_range();
        let give = self.ui_f32(EDGE_GIVE);
        let Some(edge) = self.library.edge.as_mut() else {
            return;
        };
        if edge.back.is_some() {
            return;
        }
        let raw = edge.from + (event.coords.x - edge.grab) as f32;
        edge.width = pulled(raw, least, most, give);
        context.set_cursor(Some(MouseCursor::SizeLeftRight));
        self.dragging.replace((item, start_event));
        context.invalidate();
    }

    /// The edge was let go: inside its range the terminal takes the width
    /// now; outside, the edge springs back first.
    pub(crate) fn release_library_edge(&mut self) {
        let (least, most) = self.library_edge_range();
        let Some(edge) = self.library.edge.as_mut() else {
            return;
        };
        let target = edge.width.clamp(least, most);
        if (target - edge.width).abs() < 0.5 {
            edge.width = target;
            self.commit_library_edge();
            return;
        }
        edge.back = Some((edge.width, target));
        edge.spring = Spring::default();
        edge.spring.to(1.0, SNAP, 0.0);
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// Where the edge was taken to is where the terminal goes, and the
    /// width is remembered for new windows.
    fn commit_library_edge(&mut self) {
        let Some(edge) = self.library.edge.take() else {
            return;
        };
        let width = match edge.back {
            Some((_, to)) => to,
            None => edge.width,
        };
        let design = (width / self.ui_f32(1.0).max(0.01)).round();
        self.library.width = Some(design);
        if let Err(err) = crate::native_settings::save_library_width(design.max(1.0) as usize) {
            log::warn!("failed to save the Library's width: {err:#}");
        }
        if let Some(window) = self.window.clone() {
            self.reflow_workspace_sidebar(&window);
            window.invalidate();
        }
    }
}

// macOS can start momentum without a scroll Ended. Release the finger
// without applying the momentum delta to the held position.
fn library_swipe_phase(
    scroll: Option<ScrollPhase>,
    momentum: Option<ScrollPhase>,
) -> Option<ScrollPhase> {
    scroll.or_else(|| matches!(momentum, Some(ScrollPhase::Began)).then_some(ScrollPhase::Ended))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn momentum_without_scroll_ended_releases_the_library_swipe() {
        assert_eq!(
            library_swipe_phase(Some(ScrollPhase::Changed), None),
            Some(ScrollPhase::Changed)
        );
        assert_eq!(
            library_swipe_phase(None, Some(ScrollPhase::Began)),
            Some(ScrollPhase::Ended)
        );
        assert_eq!(library_swipe_phase(None, Some(ScrollPhase::Changed)), None);
        assert_eq!(
            library_swipe_phase(Some(ScrollPhase::Cancelled), None),
            Some(ScrollPhase::Cancelled)
        );
    }

    fn space(id: &str, domain: Option<&str>) -> SpaceView {
        SpaceView {
            id: id.to_string(),
            name: id.to_string(),
            is_active: false,
            is_default: false,
            is_occupied_by_other_window: false,
            is_remote: domain.is_some(),
            domain: domain.map(str::to_string),
            is_domain_attached: domain.is_some(),
            shown_as_folders: false,
        }
    }

    fn ids(machines: &[(Option<String>, Vec<SpaceView>)]) -> Vec<(Option<&str>, Vec<&str>)> {
        machines
            .iter()
            .map(|(domain, spaces)| {
                (
                    domain.as_deref(),
                    spaces.iter().map(|space| space.id.as_str()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn this_computer_comes_first_then_servers_as_they_first_appear() {
        let grouped = machines(vec![
            space("a1", Some("server-a")),
            space("l1", None),
            space("b1", Some("server-b")),
            space("a2", Some("server-a")),
            space("l2", None),
        ]);
        assert_eq!(
            ids(&grouped),
            vec![
                (None, vec!["l1", "l2"]),
                (Some("server-a"), vec!["a1", "a2"]),
                (Some("server-b"), vec!["b1"]),
            ]
        );
    }

    #[test]
    fn a_server_shown_as_folders_has_no_card_but_for_the_space_the_window_is_in() {
        let folded = |id: &str, active: bool| SpaceView {
            shown_as_folders: true,
            is_active: active,
            ..space(id, Some("server-a"))
        };
        assert_eq!(
            ids(&machines(vec![space("l1", None), folded("a1", false)])),
            vec![(None, vec!["l1"])]
        );
        assert_eq!(
            ids(&machines(vec![space("l1", None), folded("a1", true)])),
            vec![(None, vec!["l1"]), (Some("server-a"), vec!["a1"])]
        );
    }

    #[test]
    fn without_a_space_of_its_own_this_computer_has_no_group() {
        assert_eq!(
            ids(&machines(vec![space("a1", Some("server-a"))])),
            vec![(Some("server-a"), vec!["a1"])]
        );
    }

    #[test]
    fn cards_are_laid_out_along_one_row_with_wider_gaps_between_machines() {
        let (starts, length) = row_layout(&[2, 1, 2], 210.0, 16.0, 30.0);
        assert_eq!(
            starts,
            vec![vec![0.0, 226.0], vec![466.0], vec![706.0, 932.0]]
        );
        assert_eq!(length, 1142.0);
        assert_eq!(row_layout(&[], 210.0, 16.0, 30.0), (vec![], 0.0));
    }

    #[test]
    fn the_sidebar_swings_out_and_round_into_its_card_without_turning_twice() {
        let (start, swing, card) = (0.0, 316.0, 86.0);
        assert_eq!(swing_x(start, swing, card, 0.0), start);
        assert!((swing_x(start, swing, card, 1.0) - card).abs() < 1e-4);
        let path: Vec<f32> = (0..=100).map(|step| swing_x(start, swing, card, step as f32 / 100.0)).collect();
        let turns = path
            .windows(3)
            .filter(|w| (w[1] - w[0]).signum() != (w[2] - w[1]).signum())
            .count();
        assert_eq!(turns, 1, "out and back, once: {path:?}");
        assert!(path.iter().cloned().fold(f32::MIN, f32::max) > card + 50.0);
        // Past its card it carries on the way it was going: further left.
        assert!(swing_x(start, swing, card, 1.02) < card);
    }

    #[test]
    fn a_spring_comes_to_rest_where_it_goes_and_a_lively_one_goes_past_first() {
        let mut spring = Spring::default();
        spring.to(1.0, LAND, 0.0);
        let mut highest = 0.0f32;
        for _ in 0..240 {
            spring.advance(1.0 / 60.0);
            highest = highest.max(spring.x);
        }
        assert!(!spring.moving());
        assert_eq!(spring.x, 1.0);
        assert!(highest > 1.005, "a little past: {highest}");

        let mut damped = Spring::default();
        damped.to(1.0, GO_BACK, 0.0);
        let mut highest = 0.0f32;
        for _ in 0..240 {
            damped.advance(1.0 / 60.0);
            highest = highest.max(damped.x);
        }
        assert!(highest <= 1.0001, "never past: {highest}");
    }

    #[test]
    fn a_delayed_spring_waits_before_it_moves() {
        let mut spring = Spring::default();
        spring.to(1.0, GO_BACK, 0.1);
        spring.advance(0.05);
        assert_eq!(spring.x, 0.0);
        assert!(spring.moving());
        spring.advance(0.2);
        assert!(spring.x > 0.0);
    }

    #[test]
    fn the_edge_gives_less_the_further_it_is_pulled() {
        assert_eq!(pulled(500.0, 360.0, 660.0, 110.0), 500.0);
        // Each further 40 pixels of pull moves it less than the last.
        let a = pulled(700.0, 360.0, 660.0, 110.0);
        let b = pulled(740.0, 360.0, 660.0, 110.0);
        assert!(a > 660.0 && b > a);
        assert!(b - a < a - 660.0);
        assert!(pulled(10_000.0, 360.0, 660.0, 110.0) < 660.0 + 110.0);
        assert!(pulled(300.0, 360.0, 660.0, 110.0) < 360.0);
    }

    #[test]
    fn a_row_that_fits_casts_no_shadow_and_one_running_past_deepens_it_as_it_does() {
        assert_eq!(overflow_strength(-40.0, 24.0), 0.0);
        assert_eq!(overflow_strength(0.0, 24.0), 0.0);
        assert_eq!(overflow_strength(12.0, 24.0), 0.5);
        assert_eq!(overflow_strength(500.0, 24.0), 1.0);
    }

    #[test]
    fn a_card_panel_off_the_window_is_recorded_on_it_and_moved_back() {
        let panel = CardPanel {
            x: -120.0,
            y: 40.0,
            width: 500,
            height: 900,
        };
        let (at, by) = panel.recorded_at();
        assert_eq!(at, (0.0, 40.0));
        assert_eq!(by, (-120.0, 0.0));
        let on = CardPanel { x: 300.0, ..panel };
        assert_eq!(on.recorded_at(), ((300.0, 40.0), (0.0, 0.0)));
    }
}
