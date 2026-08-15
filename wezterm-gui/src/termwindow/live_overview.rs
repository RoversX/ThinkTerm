use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::content_view::{
    ContentView, ContentViewPresentation, ContentViewResponse, ContentViewTypography,
    TerminalPreviewPaneSnapshot, TerminalPreviewRequest, TerminalPreviewSnapshot,
};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::{TermWindow, TermWindowNotif};
use crate::ui::anim::{self, Easing, Timeline};
use crate::ui::{
    draw_button_on_layer, draw_icon_button, draw_icon_button_on_layer, draw_scrollbar_on_layer,
    precise_wheel_delta_pixels, wheel_delta_pixels, ButtonSpec, ControlState, DrawContext,
    InteractionState, ScrollState, UiContext, UiPalette, UiTokens, WidgetKind,
};
use crate::workspace_threads;
use fluent_bundle::FluentArgs;
use mux::domain::DomainState;
use mux::pane::{CachePolicy, CloseReason, PaneId};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::{PositionedSplit, TabId};
use mux::Mux;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wezterm_client::domain::FrontendRecoverySlot;
use wezterm_font::LoadedFont;
use wezterm_term::{KeyCode, KeyModifiers, TerminalSize};
use window::color::LinearRgba;
use window::{Appearance, MouseEventKind as WMEK, MousePress, RectF, WindowOps};

pub(crate) const LIVE_OVERVIEW_CONTENT_VIEW_KEY: &str = "live-overview";

// All geometry is authored in ThinkTerm's 2x macOS backing-pixel design grid
// and converted through DrawContext::px. 3000 design pixels equal 1500 logical
// window pixels on every supported DPI; this is a breakpoint, not a content
// max-width.
const PAGE_PAD_X: f32 = 84.0;
const PAGE_PAD_TOP: f32 = 40.0;
const PAGE_PAD_BOTTOM: f32 = 44.0;
const GROUP_HEADER_HEIGHT: f32 = 42.0;
const GROUP_TITLE_CARD_GAP: f32 = 8.0;
const GROUP_GAP: f32 = 48.0;
const CARD_GAP: f32 = 16.0;
const CARD_MIN_WIDTH: f32 = 400.0;
const CARD_ORPHAN_COMFORT_WIDTH: f32 = 480.0;
const CARD_MAX_WIDTH: f32 = 640.0;
const CARD_HEADER_HEIGHT: f32 = 44.0;
const CARD_RADIUS: f32 = 18.0;
const CARD_INSET: f32 = 8.0;
const CARD_CLOSE_BUTTON_SIZE: f32 = 32.0;
const CARD_CLOSE_RIGHT_PAD: f32 = 8.0;
const CARD_TITLE_CLOSE_GAP: f32 = 8.0;
/// Space between the thread name and the command it is running.
const RUNNING_LABEL_GAP: f32 = 14.0;
/// Below this there is no room to say anything useful, so say nothing rather
/// than showing two or three clipped letters.
const RUNNING_LABEL_MIN_WIDTH: f32 = 56.0;
const PREVIEW_RADIUS: f32 = 12.0;
const CLOSE_BUTTON_SIZE: f32 = 44.0;
const CONFIRM_MIN_WIDTH: f32 = 640.0;
const CONFIRM_MAX_WIDTH: f32 = 880.0;
const CONFIRM_SIDE_MARGIN: f32 = 64.0;
const CONFIRM_PADDING: f32 = 36.0;
const CONFIRM_RADIUS: f32 = 24.0;
const CONFIRM_TEXT_GAP: f32 = 18.0;
const CONFIRM_BUTTON_GAP: f32 = 12.0;
/// Bounds on the shape a card may take. A card is a picture of the terminal,
/// so the terminal's own proportions decide this and the clamp only guards
/// against a degenerate window. The lower bound used to sit at 1.2, which is
/// wider than a terminal gets as soon as a sidebar is open -- at 2704px wide
/// with both sidebars out the terminal is 1699x1622, or 1.05, and every card
/// was being stretched 15% wider than the thing it was a picture of.
const HOST_PREVIEW_ASPECT_MIN: f32 = 0.55;
const HOST_PREVIEW_ASPECT_MAX: f32 = 3.4;
const MAX_COLUMNS: usize = 5;
const MAX_COLUMNS_BELOW_WIDE_BREAKPOINT: usize = 4;
const FIVE_COLUMN_WINDOW_WIDTH: f32 = 3000.0;
const LIVE_RESIZE_PREVIEW_INTERVAL: Duration = Duration::from_millis(33);
/// How often a card re-asks what its terminal is running.
///
/// `CachePolicy::AllowStale` is not the cheap read its name suggests: it takes
/// a lock, clones a `CachedLeaderInfo` with its paths, and spawns a thread when
/// the entry has expired. Asking once per card per frame put that on the render
/// thread sixty times a second, for cards scrolled out of sight as much as
/// visible ones. A label naming the program at the prompt does not need to be
/// fresher than this.
const RUNNING_LABEL_REFRESH: Duration = Duration::from_millis(1000);
const SCROLLBAR_VISIBLE_INTERVAL: Duration = Duration::from_millis(900);
/// The tail of that interval is spent fading rather than shown at full
/// strength and then cut.
const SCROLLBAR_FADE: Duration = anim::SHORT;
const SCROLL_MASK_FADE_HEIGHT: f32 = 32.0;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct LiveThreadKey {
    space_id: String,
    thread_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalPreviewFingerprint {
    tab_id: TabId,
    tab_size: TerminalSize,
    panes: Vec<TerminalPreviewPaneFingerprint>,
    splits: Vec<PositionedSplit>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalPreviewPaneFingerprint {
    pane_id: PaneId,
    index: usize,
    is_active: bool,
    is_zoomed: bool,
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    dimensions: RenderableDimensions,
    seqno: usize,
    palette_identity: u64,
    cursor: StableCursorPosition,
}

#[derive(Clone, Debug)]
struct CachedPreview<T> {
    fingerprint: TerminalPreviewFingerprint,
    snapshot: Arc<T>,
    captured_at: Instant,
}

#[derive(Clone, Debug)]
struct LiveCard {
    key: LiveThreadKey,
    title: String,
    /// What this terminal is running right now, if it is running anything.
    /// The thumbnail below says what the screen looks like; at card size that
    /// is texture rather than information, and this is the line that actually
    /// answers "what is happening here".
    running: Option<String>,
    tab_id: TabId,
    active: bool,
}

#[derive(Clone, Debug)]
struct LiveGroup {
    name: String,
    offline: bool,
    cards: Vec<LiveCard>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverviewAction {
    CloseOverview,
    OpenThread(TabId),
    CloseTab(TabId),
    ConfirmCloseTab,
    CancelCloseTab,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingClose {
    tab_id: TabId,
    title: String,
}

#[derive(Clone, Copy, Debug)]
struct GroupGrid {
    columns: usize,
    rows: usize,
    card_width: f32,
    card_height: f32,
}

#[derive(Clone, Copy, Debug)]
struct GroupLayout {
    header_y: f32,
    cards_y: f32,
    grid: GroupGrid,
}

/// Where a card is travelling from and to, so that a change of layout is
/// covered rather than jumped.
///
/// The rectangles are in content space -- the scroll offset and the viewport
/// origin are applied afterwards. Scrolling moves every card on every frame,
/// and diffing in screen space would read that as the whole grid reflowing.
#[derive(Clone, Debug)]
struct CardMotion {
    from: RectF,
    to: RectF,
    travel: Timeline,
}

impl CardMotion {
    fn settled(now: Instant, rect: RectF) -> Self {
        Self {
            from: rect,
            to: rect,
            travel: Timeline::settled(now, 1.0),
        }
    }

    fn current(&self, now: Instant) -> RectF {
        lerp_rect(self.from, self.to, self.travel.value(now))
    }
}

fn lerp_rect(from: RectF, to: RectF, t: f32) -> RectF {
    let lerp = |a: f32, b: f32| a + (b - a) * t;
    euclid::rect(
        lerp(from.origin.x, to.origin.x),
        lerp(from.origin.y, to.origin.y),
        lerp(from.size.width, to.size.width),
        lerp(from.size.height, to.size.height),
    )
}

/// Sub-pixel drift is not a reflow. Without a threshold, rounding in the grid
/// arithmetic would restart the travel on frames where nothing moved.
fn rect_moved(from: RectF, to: RectF) -> bool {
    let moved = |a: f32, b: f32| (a - b).abs() > 0.5;
    moved(from.origin.x, to.origin.x)
        || moved(from.origin.y, to.origin.y)
        || moved(from.size.width, to.size.width)
        || moved(from.size.height, to.size.height)
}

#[derive(Clone, Copy, Debug)]
struct CardChrome {
    preview: RectF,
    clip: RectF,
    fill: LinearRgba,
    border: LinearRgba,
}

#[derive(Clone, Copy, Debug)]
struct OverviewColors {
    surface_top_left: LinearRgba,
    surface_top_right: LinearRgba,
    surface_bottom_left: LinearRgba,
    surface_bottom_right: LinearRgba,
    card: LinearRgba,
    card_hover: LinearRgba,
    card_pressed: LinearRgba,
    shadow: LinearRgba,
    preview: LinearRgba,
    preview_border: LinearRgba,
    active_border: LinearRgba,
    modal_scrim: LinearRgba,
}

pub(crate) struct LiveOverviewView {
    owner_id: u64,
    active: Option<LiveThreadKey>,
    host_preview_aspect: f32,
    scroll: ScrollState,
    /// Whether the active thread's card has been brought into view. Done once,
    /// on the first frame that has a layout to measure against.
    revealed_active: bool,
    last_ui_scale: f32,
    viewport: RectF,
    widgets: UiContext<OverviewAction>,
    interaction: InteractionState<OverviewAction>,
    card_keys: HashMap<TabId, LiveThreadKey>,
    card_titles: HashMap<TabId, String>,
    pending_close: Option<PendingClose>,
    running_labels: HashMap<TabId, (Instant, Option<String>)>,
    snapshot_cache: HashMap<LiveThreadKey, CachedPreview<TerminalPreviewSnapshot>>,
    previews: Vec<TerminalPreviewRequest>,
    preview_chrome: Vec<CardChrome>,
    /// Where each visible card shows its terminal, so an opening overview can
    /// be told what a given terminal is about to become. Only cards that were
    /// actually laid out this frame appear here.
    card_preview_rects: HashMap<TabId, RectF>,
    /// Terminal currently travelling to or from its card. Its card is laid out
    /// and drawn as usual, but left empty: the travelling copy is the one the
    /// eye is following, and a second copy waiting in the slot is what made
    /// the transition look like two layers.
    terminal_in_flight: Option<TabId>,
    visible_panes: HashSet<PaneId>,
    live_resizing: bool,
    card_motion: HashMap<LiveThreadKey, CardMotion>,
    card_motion_running: bool,
    scroll_animating: bool,
    next_preview_refresh: Option<Instant>,
    scrollbar_visible_until: Option<Instant>,
}

impl LiveOverviewView {
    pub(crate) fn new(
        owner_id: u64,
        active_space_id: &str,
        active_workspace: &str,
        host_preview_aspect: f32,
    ) -> Self {
        Self {
            owner_id,
            active: workspace_threads::thread_id_for_workspace(active_space_id, active_workspace)
                .map(|thread_id| LiveThreadKey {
                    space_id: active_space_id.to_string(),
                    thread_id,
                }),
            // Extremely narrow/tall terminals still need a useful overview;
            // within this safety range every card keeps the exact same host
            // aspect instead of inheriting a source mux tab's split geometry.
            host_preview_aspect: host_preview_aspect.clamp(HOST_PREVIEW_ASPECT_MIN, HOST_PREVIEW_ASPECT_MAX),
            scroll: ScrollState::new(),
            revealed_active: false,
            last_ui_scale: 1.0,
            viewport: euclid::rect(0.0, 0.0, 0.0, 0.0),
            widgets: UiContext::default(),
            interaction: InteractionState::default(),
            card_keys: HashMap::new(),
            card_titles: HashMap::new(),
            pending_close: None,
            running_labels: HashMap::new(),
            snapshot_cache: HashMap::new(),
            previews: Vec::new(),
            preview_chrome: Vec::new(),
            card_preview_rects: HashMap::new(),
            terminal_in_flight: None,
            visible_panes: HashSet::new(),
            live_resizing: false,
            card_motion: HashMap::new(),
            card_motion_running: false,
            scroll_animating: false,
            next_preview_refresh: None,
            scrollbar_visible_until: None,
        }
    }

    /// Where to draw a card this frame, given where the grid says it belongs.
    ///
    /// A card that has moved travels to its new home instead of appearing
    /// there. Cards seen for the first time -- and every card while the window
    /// is being dragged, where an animation would only ever lag behind the
    /// window edge -- are placed directly.
    fn settle_card_rect(&mut self, key: &LiveThreadKey, target: RectF, now: Instant) -> RectF {
        let snap = self.live_resizing;
        match self.card_motion.get_mut(key) {
            None => {
                self.card_motion
                    .insert(key.clone(), CardMotion::settled(now, target));
                target
            }
            Some(motion) => {
                if snap {
                    *motion = CardMotion::settled(now, target);
                    return target;
                }
                if rect_moved(motion.to, target) {
                    // Depart from where the card is right now, not from where
                    // the previous travel was headed: closing a second card
                    // while the first reflow is still moving must not send
                    // everything backwards before it sets off again.
                    motion.from = motion.current(now);
                    motion.to = target;
                    motion.travel = Timeline::progress(now, anim::SHORT, Easing::OutCubic);
                }
                motion.current(now)
            }
        }
    }

    /// What `tab_id` is running, from cache unless it has gone stale.
    fn running_label(&mut self, tab_id: TabId, now: Instant) -> Option<String> {
        if let Some((asked_at, label)) = self.running_labels.get(&tab_id) {
            if now.saturating_duration_since(*asked_at) < RUNNING_LABEL_REFRESH {
                return label.clone();
            }
        }
        let label = foreground_process_name(tab_id);
        self.running_labels.insert(tab_id, (now, label.clone()));
        label
    }

    fn collect_groups(&mut self, now: Instant) -> Vec<LiveGroup> {
        let mux = Mux::get();
        let live_workspaces = mux.iter_workspaces();
        let mut groups = Vec::new();

        for space in workspace_threads::spaces_for_window(self.owner_id) {
            let offline = space.domain.as_deref().is_some_and(|domain_name| {
                mux.get_domain_by_name(domain_name)
                    .is_none_or(|domain| domain.state() != DomainState::Attached)
            });
            let mut cards = Vec::new();

            if !offline {
                for project_id in workspace_threads::ordered_project_ids(&space.id) {
                    for thread_id in workspace_threads::ordered_thread_ids(&project_id) {
                        let Some(state) = workspace_threads::thread_connection_state(
                            &thread_id,
                            &live_workspaces,
                        ) else {
                            continue;
                        };
                        if state.space_id != space.id || !state.is_live {
                            continue;
                        }

                        let key = LiveThreadKey {
                            space_id: space.id.clone(),
                            thread_id: state.thread_id.clone(),
                        };
                        let Some(tab_id) = live_tab_for_workspace(&state.workspace_name) else {
                            continue;
                        };

                        cards.push(LiveCard {
                            active: self.active.as_ref() == Some(&key),
                            key,
                            title: card_title(&state.project_name, &state.thread_name),
                            running: self.running_label(tab_id, now),
                            tab_id,
                        });
                    }
                }
            }

            // Online empty Spaces add no information to a live-only overview.
            // Keep an offline remote Space so the missing cards have an
            // explicit cause rather than silently displaying stale snapshots.
            if !cards.is_empty() || offline {
                groups.push(LiveGroup {
                    name: space.name,
                    offline,
                    cards,
                });
            }
        }

        retain_running_labels(&mut self.running_labels, &groups);

        groups
    }

    /// Where the list has to sit for the active thread's card to be on screen,
    /// or `None` if it already is -- or if there is no active card to find.
    ///
    /// This is not only about orientation. A card outside the viewport is
    /// skipped before it registers a landing rectangle, and a terminal with no
    /// landing rectangle has nowhere to go: `terminal_landing_rect` returns
    /// `None`, the flight falls back to the whole window, and the shrink
    /// degenerates into a fade in place. Revealing the card is what gives the
    /// transition a destination.
    ///
    /// Centres the card rather than scrolling the least possible distance. This
    /// runs before the first row is drawn, so there is no motion to keep small
    /// -- only the question of where the eye should land.
    fn offset_revealing_active(
        &self,
        groups: &[LiveGroup],
        layouts: &[GroupLayout],
        content_x: f32,
        content_width: f32,
        gap: f32,
    ) -> Option<f32> {
        let viewport_height = self.viewport.size.height;
        let max_offset = self.scroll.max_offset();
        if viewport_height <= 0.0 || max_offset <= 0.0 {
            return None;
        }
        let (layout, index, count) = groups.iter().zip(layouts).find_map(|(group, layout)| {
            group
                .cards
                .iter()
                .position(|card| card.active)
                .map(|index| (layout, index, group.cards.len()))
        })?;
        let rect = card_rect(
            index,
            count,
            layout.grid,
            content_x,
            content_width,
            layout.cards_y,
            gap,
        );
        // Card rectangles are in content space, so the band on screen right now
        // is `[offset, offset + viewport_height]`.
        let offset = self.scroll.offset;
        if rect.min_y() >= offset && rect.max_y() <= offset + viewport_height {
            return None;
        }
        let desired = if rect.size.height >= viewport_height {
            // Taller than the viewport: showing its top beats centring a card
            // whose header would then be cut off.
            rect.min_y()
        } else {
            rect.min_y() - (viewport_height - rect.size.height) / 2.0
        };
        Some(desired.clamp(0.0, max_offset))
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_impl(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        settings_font: &Rc<LoadedFont>,
        group_font: &Rc<LoadedFont>,
        card_font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        self.last_ui_scale = ctx.scale();
        self.widgets.clear();
        self.card_keys.clear();
        self.card_titles.clear();
        self.previews.clear();
        self.preview_chrome.clear();
        self.card_preview_rects.clear();
        self.visible_panes.clear();
        self.next_preview_refresh = None;

        let appearance = crate::native_settings::effective_appearance();
        let colors = overview_colors(appearance);

        let pad = horizontal_page_pad(ctx, area);
        let content_x = area.origin.x + pad;
        let content_width = (area.size.width - pad * 2.0).max(1.0);
        let viewport_top = area.origin.y + ctx.px(PAGE_PAD_TOP);
        let viewport_bottom = (area.max_y() - ctx.px(PAGE_PAD_BOTTOM)).max(viewport_top);
        self.viewport = euclid::rect(
            content_x,
            viewport_top,
            content_width,
            viewport_bottom - viewport_top,
        );

        let settings_line_height =
            crate::utilsprites::RenderMetrics::with_font_metrics(&settings_font.metrics())
                .cell_size
                .height as f32;
        let group_line_height =
            crate::utilsprites::RenderMetrics::with_font_metrics(&group_font.metrics())
                .cell_size
                .height as f32;
        let card_line_height =
            crate::utilsprites::RenderMetrics::with_font_metrics(&card_font.metrics())
                .cell_size
                .height as f32;
        let group_header_height = ctx
            .px(GROUP_HEADER_HEIGHT)
            .max(group_line_height + ctx.px(12.0));
        let title_card_gap = ctx.px(GROUP_TITLE_CARD_GAP);
        let card_header_height = ctx
            .px(CARD_HEADER_HEIGHT)
            .max(card_line_height + ctx.px(18.0));
        let card_inset = ctx.px(CARD_INSET);
        let gap = ctx.px(CARD_GAP);
        let group_gap = ctx.px(GROUP_GAP);
        let max_columns = max_columns_for_surface_width(
            ctx.dimensions.pixel_width as f32,
            ctx.px(FIVE_COLUMN_WINDOW_WIDTH),
        );

        let now = Instant::now();
        let groups = self.collect_groups(now);
        let refresh_interval = self.live_resizing.then_some(LIVE_RESIZE_PREVIEW_INTERVAL);
        // Advance every card's travel once, before anything samples it.
        for motion in self.card_motion.values_mut() {
            motion.travel.advance(now);
        }
        // The glide a notched wheel was given, and any smoothing towards a
        // programmatic target, both live in the scroll state.
        self.scroll_animating = self.scroll.advance_animation(now);
        if self.scroll_animating {
            self.reveal_scrollbar(now);
        }
        let mut warm_keys = HashSet::new();
        let mut seen_keys = HashSet::new();
        if groups.is_empty() {
            self.scroll.set_extents(self.viewport.size.height, 0.0);
            self.paint_empty(ctx, layers, palette, settings_font)?;
        } else {
            let counts = groups
                .iter()
                .map(|group| group.cards.len())
                .collect::<Vec<_>>();
            let (layouts, content_height) = group_layouts(
                &counts,
                content_width,
                max_columns,
                ctx.px(CARD_MIN_WIDTH),
                ctx.px(CARD_ORPHAN_COMFORT_WIDTH),
                ctx.px(CARD_MAX_WIDTH),
                gap,
                group_header_height,
                title_card_gap,
                card_header_height,
                card_inset,
                self.host_preview_aspect,
                group_gap,
            );
            self.scroll
                .set_extents(self.viewport.size.height, content_height);
            // A brand-new overview starts at the top of the list, which leaves
            // the active thread's card off screen as soon as the grid is taller
            // than the viewport. Reveal it here, before the first row is drawn,
            // so that this frame's `card_preview_rects` already carries its
            // landing rectangle and the arriving terminal has somewhere to
            // shrink into.
            if !self.revealed_active {
                self.revealed_active = true;
                if let Some(offset) =
                    self.offset_revealing_active(&groups, &layouts, content_x, content_width, gap)
                {
                    self.scroll.scroll_by(offset - self.scroll.offset);
                }
            }

            for (group, layout) in groups.iter().zip(layouts) {
                let heading_y = self.viewport.origin.y + layout.header_y - self.scroll.offset;
                if row_visible(heading_y, group_header_height, self.viewport) {
                    let heading_text_y =
                        heading_y + (group_header_height - group_line_height).max(0.0) / 2.0;
                    let title_width = if layout.header_y == 0.0 {
                        (content_width - ctx.px(CLOSE_BUTTON_SIZE + 20.0)).max(1.0)
                    } else {
                        content_width
                    };
                    ctx.draw_text(
                        layers,
                        group_font,
                        content_x,
                        heading_text_y,
                        &group.name,
                        palette.text,
                        title_width,
                    )?;
                    if group.offline {
                        self.paint_offline_badge(
                            ctx,
                            layers,
                            palette,
                            settings_font,
                            content_x,
                            heading_y
                                + (group_header_height
                                    - (settings_line_height + ctx.px(8.0)).max(ctx.px(24.0)))
                                .max(0.0)
                                    / 2.0,
                            &group.name,
                            group_font,
                            title_width,
                            settings_line_height,
                        )?;
                    }
                }

                for (index, card) in group.cards.iter().enumerate() {
                    // Content space: `cards_y` is measured from the top of the
                    // scrollable content, so the same card keeps the same
                    // rectangle no matter where the list is scrolled to.
                    let target = card_rect(
                        index,
                        group.cards.len(),
                        layout.grid,
                        content_x,
                        content_width,
                        layout.cards_y,
                        gap,
                    );
                    seen_keys.insert(card.key.clone());
                    let rect = self
                        .settle_card_rect(&card.key, target, now)
                        .translate(euclid::vec2(
                            0.0,
                            self.viewport.origin.y - self.scroll.offset,
                        ));

                    // Keep one complete row warm above and below the viewport
                    // so a scroll does not reveal an uncaptured thumbnail. All
                    // other cards remain metadata-only.
                    let overscan = layout.grid.card_height + gap;
                    let snapshot = if card_is_warm(rect, self.viewport, overscan) {
                        warm_keys.insert(card.key.clone());
                        let fingerprint = terminal_preview_fingerprint(card.tab_id);
                        let (snapshot, refresh_due) = resolve_snapshot(
                            &mut self.snapshot_cache,
                            &card.key,
                            fingerprint,
                            now,
                            refresh_interval,
                            || capture_terminal_snapshot(card.tab_id),
                        );
                        if let Some(refresh_due) = refresh_due {
                            self.next_preview_refresh = Some(
                                self.next_preview_refresh
                                    .map_or(refresh_due, |current| current.min(refresh_due)),
                            );
                        }
                        snapshot
                    } else {
                        None
                    };

                    let Some(visible) = rect.intersection(&self.viewport) else {
                        continue;
                    };
                    if visible.size.width <= 1.0 || visible.size.height <= 1.0 {
                        continue;
                    }

                    let tab_id = card.tab_id;
                    let action = OverviewAction::OpenThread(tab_id);
                    let close_action = OverviewAction::CloseTab(tab_id);
                    let card_hovered = matches!(
                        self.interaction.hovered,
                        Some(OverviewAction::OpenThread(id) | OverviewAction::CloseTab(id))
                            if id == tab_id
                    );
                    let fill = if self.interaction.pressed == Some(action) {
                        colors.card_pressed
                    } else if card_hovered {
                        colors.card_hover
                    } else {
                        colors.card
                    };
                    let border = if card.active {
                        colors.active_border
                    } else {
                        palette.control_border
                    };
                    // Keep the card's real geometry even when the viewport
                    // intersects only part of it. The fixed header/footer masks
                    // clip the overflow in the final pass; flattening `visible`
                    // into a plain rectangle destroys corners that are still on
                    // screen (notably the top corners of the last row).
                    ctx.draw_elevated_surface(
                        layers,
                        0,
                        rect,
                        fill,
                        border,
                        colors.shadow,
                        ctx.px(CARD_RADIUS),
                    )?;

                    let icon_size = ctx.px(20.0);
                    let icon_x = rect.origin.x + ctx.px(16.0);
                    let icon_y = rect.origin.y + (card_header_height - icon_size) / 2.0;
                    let title_y =
                        rect.origin.y + (card_header_height - card_line_height).max(0.0) / 2.0;
                    let close_size = ctx.px(CARD_CLOSE_BUTTON_SIZE);
                    let close_x = rect.max_x() - ctx.px(CARD_CLOSE_RIGHT_PAD) - close_size;
                    let close_y = rect.origin.y + (card_header_height - close_size) / 2.0;
                    let header_fully_visible =
                        row_fully_visible(rect.origin.y, card_header_height, self.viewport);
                    if row_fully_visible(title_y, card_line_height, self.viewport) {
                        ctx.draw_svg_icon(
                            layers,
                            SvgIcon::SquareTerminal,
                            icon_x,
                            icon_y,
                            icon_size,
                            palette.secondary_text,
                        )?;
                        let text_x = icon_x + icon_size + ctx.px(10.0);
                        let text_limit = (close_x - ctx.px(CARD_TITLE_CLOSE_GAP) - text_x).max(1.0);
                        ctx.draw_text(
                            layers,
                            card_font,
                            text_x,
                            title_y,
                            &card.title,
                            palette.text,
                            text_limit,
                        )?;
                        if let Some(running) = card.running.as_ref() {
                            // Only once the name of the thread has been given
                            // its space: which terminal this is comes first,
                            // what it is doing second.
                            let title_width =
                                ctx.measure_text_width(card_font, &card.title).min(text_limit);
                            let running_x = text_x + title_width + ctx.px(RUNNING_LABEL_GAP);
                            let running_limit = text_x + text_limit - running_x;
                            if running_limit >= ctx.px(RUNNING_LABEL_MIN_WIDTH) {
                                ctx.draw_text(
                                    layers,
                                    card_font,
                                    running_x,
                                    title_y,
                                    running,
                                    palette.muted_text,
                                    running_limit,
                                )?;
                            }
                        }
                    }

                    let preview_width = (rect.size.width - card_inset * 2.0).max(1.0);
                    let preview = euclid::rect(
                        rect.origin.x + card_inset,
                        rect.origin.y + card_header_height,
                        preview_width,
                        preview_width / self.host_preview_aspect,
                    );
                    let in_flight = self.terminal_in_flight == Some(tab_id);
                    // A card whose terminal is currently flying to it is left
                    // empty, but not neutral: the panel takes that terminal's
                    // own background so the arriving picture settles onto the
                    // same colour it is already showing. Filling it with the
                    // generic preview grey put a step in the handover, and the
                    // step read as a flash.
                    let preview_fill = if in_flight {
                        snapshot
                            .as_ref()
                            .and_then(|snapshot| snapshot.panes.first())
                            .map(|pane| pane.palette.background.to_linear())
                            .unwrap_or(colors.preview)
                    } else {
                        colors.preview
                    };
                    if let Some(clip) = preview.intersection(&self.viewport) {
                        if clip.size.width > 1.0 && clip.size.height > 1.0 {
                            ctx.draw_rounded_rect(
                                layers,
                                0,
                                preview.origin.x,
                                preview.origin.y,
                                preview.size.width,
                                preview.size.height,
                                preview_fill,
                                ctx.px(PREVIEW_RADIUS),
                            )?;
                            if let Some(snapshot) = snapshot.as_ref().filter(|_| !in_flight) {
                                self.previews.push(TerminalPreviewRequest {
                                    snapshot: Arc::clone(snapshot),
                                    area: preview,
                                    clip,
                                    hold_scale: self.live_resizing,
                                });
                                self.visible_panes
                                    .extend(snapshot.panes.iter().map(|pane| pane.pane_id));
                            }
                            self.preview_chrome.push(CardChrome {
                                preview,
                                clip,
                                fill,
                                border: colors.preview_border,
                            });
                            self.card_preview_rects.insert(tab_id, preview);
                        }
                    }

                    self.card_keys.insert(tab_id, card.key.clone());
                    self.card_titles.insert(tab_id, card.title.clone());
                    self.widgets.push(visible, WidgetKind::SidebarRow, action);
                    if header_fully_visible {
                        if card_hovered {
                            draw_icon_button(
                                ctx,
                                layers,
                                &mut self.widgets,
                                &self.interaction,
                                palette,
                                close_x,
                                close_y,
                                close_size,
                                SvgIcon::X,
                                close_action,
                            )?;
                        } else {
                            self.widgets.push(
                                RectF::new(
                                    euclid::point2(close_x, close_y),
                                    euclid::size2(close_size, close_size),
                                ),
                                WidgetKind::Button,
                                close_action,
                            );
                        }
                    }
                }
            }
        }
        if self.scroll.max_offset() <= 0.0 {
            self.scrollbar_visible_until = None;
        }
        self.snapshot_cache.retain(|key, _| warm_keys.contains(key));
        // The running-label cache is pruned in `collect_groups`, against every
        // live tab rather than against the cards that happened to be drawn.
        // A card that is gone has nowhere left to travel to; keeping its
        // motion would also mean a reopened thread animating in from wherever
        // it happened to sit last time.
        self.card_motion.retain(|key, _| seen_keys.contains(key));
        self.card_motion_running = self
            .card_motion
            .values()
            .any(|motion| motion.travel.is_running());

        Ok(())
    }

    fn paint_scroll_masks(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
    ) -> anyhow::Result<()> {
        let surface = euclid::rect(
            0.0,
            0.0,
            ctx.dimensions.pixel_width as f32,
            ctx.dimensions.pixel_height as f32,
        );
        let colors = overview_colors(crate::native_settings::effective_appearance());
        let top = self
            .viewport
            .min_y()
            .clamp(surface.min_y(), surface.max_y());
        let bottom = self
            .viewport
            .max_y()
            .clamp(surface.min_y(), surface.max_y());

        draw_surface_gradient_slice(
            ctx,
            layers,
            surface,
            euclid::rect(
                surface.min_x(),
                surface.min_y(),
                surface.size.width,
                top - surface.min_y(),
            ),
            colors,
            1.0,
            1.0,
        )?;
        draw_surface_gradient_slice(
            ctx,
            layers,
            surface,
            euclid::rect(
                surface.min_x(),
                bottom,
                surface.size.width,
                surface.max_y() - bottom,
            ),
            colors,
            1.0,
            1.0,
        )?;

        let fade = ctx
            .px(SCROLL_MASK_FADE_HEIGHT)
            .min(self.viewport.size.height / 3.0);
        if self.scroll.offset > 0.5 && fade > 0.0 {
            draw_surface_gradient_slice(
                ctx,
                layers,
                surface,
                euclid::rect(surface.min_x(), top, surface.size.width, fade),
                colors,
                1.0,
                0.0,
            )?;
        }
        if self.scroll.offset < self.scroll.max_offset() - 0.5 && fade > 0.0 {
            draw_surface_gradient_slice(
                ctx,
                layers,
                surface,
                euclid::rect(surface.min_x(), bottom - fade, surface.size.width, fade),
                colors,
                0.0,
                1.0,
            )?;
        }
        Ok(())
    }

    fn paint_fixed_controls(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
    ) -> anyhow::Result<()> {
        let scrollbar_opacity = self.scrollbar_opacity_at(Instant::now());
        if scrollbar_opacity > 0.0 {
            let scrollbar_area = euclid::rect(
                self.viewport.origin.x,
                self.viewport.origin.y,
                self.viewport.size.width,
                self.viewport.size.height,
            );
            let mut palette = palette;
            palette.scrollbar_thumb = palette.scrollbar_thumb.mul_alpha(scrollbar_opacity);
            draw_scrollbar_on_layer(
                ctx,
                layers,
                palette,
                UiTokens::for_dpi(ctx.dimensions.dpi),
                scrollbar_area,
                self.scroll,
                2,
            )?;
        }

        // Register this last so a long first Space title can never steal the
        // close target. Drawing it after the scroll mask keeps the shared icon
        // button visible without hand-authoring a second control style.
        let pad = horizontal_page_pad(ctx, area);
        let close_size = ctx.px(CLOSE_BUTTON_SIZE);
        draw_icon_button_on_layer(
            ctx,
            layers,
            &mut self.widgets,
            &self.interaction,
            palette,
            area.max_x() - pad - close_size,
            area.origin.y + ctx.px(14.0),
            close_size,
            SvgIcon::X,
            OverviewAction::CloseOverview,
            2,
        )
    }

    fn reveal_scrollbar(&mut self, now: Instant) {
        if self.scroll.max_offset() > 0.0 {
            self.scrollbar_visible_until = Some(now + SCROLLBAR_VISIBLE_INTERVAL);
        }
    }

    /// The scrollbar is transient, and used to vanish between one frame and
    /// the next. Spend the tail of its visible interval fading instead, so
    /// what the eye catches is something leaving rather than something
    /// disappearing.
    fn scrollbar_opacity_at(&self, now: Instant) -> f32 {
        if self.scroll.max_offset() <= 0.0 {
            return 0.0;
        }
        let Some(until) = self.scrollbar_visible_until else {
            return 0.0;
        };
        let Some(remaining) = until.checked_duration_since(now) else {
            return 0.0;
        };
        if remaining >= SCROLLBAR_FADE {
            return 1.0;
        }
        Easing::Smooth.apply(remaining.as_secs_f32() / SCROLLBAR_FADE.as_secs_f32())
    }

    fn next_frame_deadline(&self, now: Instant) -> Option<Instant> {
        // A travelling card wants the next frame the display will give it.
        // Asking for `now` rather than naming an interval leaves the pacing to
        // the repaint scheduler and the backend, which already throttle to
        // this panel's refresh rate.
        let motion_deadline = (self.card_motion_running || self.scroll_animating).then_some(now);
        // Before the fade begins one frame is enough -- the one that starts
        // it. Inside the fade every frame counts.
        let scrollbar_deadline = self.scrollbar_visible_until.and_then(|until| {
            let fade_from = until.checked_sub(SCROLLBAR_FADE).unwrap_or(until);
            if now < fade_from {
                Some(fade_from)
            } else if now < until {
                Some(now)
            } else {
                None
            }
        });
        [
            motion_deadline,
            self.next_preview_refresh,
            scrollbar_deadline,
        ]
        .iter()
        .flatten()
        .copied()
        .min()
    }

    fn paint_empty(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        let icon_size = ctx.px(42.0);
        let center_x = self.viewport.origin.x + self.viewport.size.width / 2.0;
        let center_y = self.viewport.origin.y + self.viewport.size.height * 0.42;
        ctx.draw_svg_icon(
            layers,
            SvgIcon::SquareTerminal,
            center_x - icon_size / 2.0,
            center_y - icon_size,
            icon_size,
            palette.muted_text,
        )?;
        let label = crate::i18n::tr("live-overview-empty");
        let width = ctx.measure_text_width(font, &label);
        ctx.draw_text(
            layers,
            font,
            center_x - width / 2.0,
            center_y + ctx.px(10.0),
            &label,
            palette.muted_text,
            width.max(1.0),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_offline_badge(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        x: f32,
        y: f32,
        group_name: &str,
        group_font: &Rc<LoadedFont>,
        available_width: f32,
        line_height: f32,
    ) -> anyhow::Result<()> {
        let group_width = ctx.measure_text_width(group_font, group_name);
        let label = crate::i18n::tr("live-overview-offline");
        let text_width = ctx.measure_text_width(font, &label);
        let badge_width = text_width + ctx.px(18.0);
        let badge_height = (line_height + ctx.px(8.0)).max(ctx.px(24.0));
        let badge_x = (x + group_width + ctx.px(14.0))
            .min(x + available_width - badge_width)
            .max(x);
        ctx.draw_rounded_rect(
            layers,
            0,
            badge_x,
            y,
            badge_width,
            badge_height,
            palette.sidebar_row_hover_bg,
            badge_height / 2.0,
        )?;
        ctx.draw_text(
            layers,
            font,
            badge_x + ctx.px(9.0),
            y + (badge_height - line_height).max(0.0) / 2.0,
            &label,
            palette.muted_text,
            text_width.max(1.0),
        )
    }

    fn action_state(&self, action: OverviewAction) -> ControlState {
        if self.interaction.pressed == Some(action) {
            ControlState::Pressed
        } else if self.interaction.hovered == Some(action) {
            ControlState::Hovered
        } else {
            ControlState::Normal
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_close_confirmation(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        let Some(pending) = self.pending_close.as_ref() else {
            return Ok(());
        };
        let pending_title = pending.title.clone();
        let colors = overview_colors(crate::native_settings::effective_appearance());
        ctx.draw_rect(
            layers,
            2,
            0.0,
            0.0,
            ctx.dimensions.pixel_width as f32,
            ctx.dimensions.pixel_height as f32,
            colors.modal_scrim,
        )?;

        let tokens = UiTokens::for_dpi(ctx.dimensions.dpi);
        let side_margin = ctx.px(CONFIRM_SIDE_MARGIN);
        let available_width = (area.size.width - side_margin).max(1.0);
        let dialog_width = (area.size.width * 0.5)
            .clamp(ctx.px(CONFIRM_MIN_WIDTH), ctx.px(CONFIRM_MAX_WIDTH))
            .min(available_width);
        let padding = ctx.px(CONFIRM_PADDING).min(dialog_width * 0.12);
        let title_height =
            crate::utilsprites::RenderMetrics::with_font_metrics(&title_font.metrics())
                .cell_size
                .height as f32;
        let body_height = crate::utilsprites::RenderMetrics::with_font_metrics(&font.metrics())
            .cell_size
            .height as f32;
        let text_gap = ctx.px(CONFIRM_TEXT_GAP);
        let dialog_height = padding * 2.0
            + title_height
            + text_gap
            + body_height
            + ctx.px(28.0)
            + tokens.control_height;
        let dialog = euclid::rect(
            area.origin.x + (area.size.width - dialog_width) / 2.0,
            area.origin.y + (area.size.height - dialog_height).max(0.0) / 2.0,
            dialog_width,
            dialog_height.min(area.size.height.max(1.0)),
        );
        ctx.draw_elevated_surface(
            layers,
            2,
            dialog,
            palette.sidebar_bg,
            palette.control_border,
            colors.shadow,
            ctx.px(CONFIRM_RADIUS),
        )?;

        let mut args = FluentArgs::new();
        args.set("title", pending_title);
        let title = crate::i18n::tr_args("live-overview-close-title", &args);
        let detail = crate::i18n::tr("live-overview-close-detail");
        let text_width = (dialog.size.width - padding * 2.0).max(1.0);
        ctx.draw_text_on_layer(
            layers,
            2,
            title_font,
            dialog.origin.x + padding,
            dialog.origin.y + padding,
            &title,
            palette.text,
            text_width,
        )?;
        ctx.draw_text_on_layer(
            layers,
            2,
            font,
            dialog.origin.x + padding,
            dialog.origin.y + padding + title_height + text_gap,
            &detail,
            palette.secondary_text,
            text_width,
        )?;

        let cancel_label = crate::i18n::tr("live-overview-close-cancel");
        let confirm_label = crate::i18n::tr("live-overview-close-confirm");
        let button_gap = ctx.px(CONFIRM_BUTTON_GAP);
        let button_available = (dialog.size.width - padding * 2.0 - button_gap).max(2.0);
        let desired_cancel = ctx.measure_text_width(font, &cancel_label) + ctx.px(36.0);
        let desired_confirm = ctx.measure_text_width(font, &confirm_label) + ctx.px(36.0);
        let (cancel_width, confirm_width) = if desired_cancel + desired_confirm <= button_available
        {
            (desired_cancel, desired_confirm)
        } else {
            (button_available / 2.0, button_available / 2.0)
        };
        let buttons_y = dialog.max_y() - padding - tokens.control_height;
        let confirm_x = dialog.max_x() - padding - confirm_width;
        let cancel_x = confirm_x - button_gap - cancel_width;
        let cancel_state = self.action_state(OverviewAction::CancelCloseTab);
        draw_button_on_layer(
            ctx,
            layers,
            font,
            &mut self.widgets,
            palette,
            ButtonSpec {
                label: &cancel_label,
                action: OverviewAction::CancelCloseTab,
                rect: euclid::rect(cancel_x, buttons_y, cancel_width, tokens.control_height),
                state: cancel_state,
                kind: WidgetKind::Button,
            },
            2,
        )?;
        let confirm_state = self.action_state(OverviewAction::ConfirmCloseTab);
        draw_button_on_layer(
            ctx,
            layers,
            font,
            &mut self.widgets,
            palette,
            ButtonSpec {
                label: &confirm_label,
                action: OverviewAction::ConfirmCloseTab,
                rect: euclid::rect(confirm_x, buttons_y, confirm_width, tokens.control_height),
                state: confirm_state,
                kind: WidgetKind::Button,
            },
            2,
        )
    }

    fn request_close_tab(&mut self, tab_id: TabId) -> ContentViewResponse {
        let Some(tab) = Mux::get().get_tab(tab_id) else {
            return ContentViewResponse::Redraw;
        };
        if tab.can_close_without_prompting(CloseReason::Tab) {
            close_tab_response(tab_id)
        } else {
            self.pending_close = Some(PendingClose {
                tab_id,
                title: self
                    .card_titles
                    .get(&tab_id)
                    .cloned()
                    .unwrap_or_else(|| "Terminal".to_string()),
            });
            self.interaction = InteractionState::default();
            ContentViewResponse::Redraw
        }
    }

    fn confirm_close_tab(&mut self) -> ContentViewResponse {
        let Some(pending) = self.pending_close.take() else {
            return ContentViewResponse::Redraw;
        };
        self.interaction = InteractionState::default();
        close_tab_response(pending.tab_id)
    }

    fn cancel_close_tab(&mut self) -> ContentViewResponse {
        self.pending_close = None;
        self.interaction = InteractionState::default();
        ContentViewResponse::Redraw
    }

    fn on_mouse_impl(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse {
        let modal_active = self.pending_close.is_some();
        let hit = self
            .widgets
            .hit_test(x, y)
            .map(|target| target.action)
            .filter(|action| {
                !modal_active
                    || matches!(
                        action,
                        OverviewAction::ConfirmCloseTab | OverviewAction::CancelCloseTab
                    )
            });
        match kind {
            WMEK::VertWheel(_) if modal_active => ContentViewResponse::Ignored,
            WMEK::VertWheel(amount) => {
                // Reached only when the wheel arrives without its full event,
                // which is every backend that has no pixel deltas to report.
                // See `on_wheel` for the trackpad path.
                let old = self.scroll.offset;
                self.scroll
                    .scroll_by(wheel_delta_pixels(amount, self.last_ui_scale));
                if (old - self.scroll.offset).abs() > 0.01 {
                    self.reveal_scrollbar(Instant::now());
                    ContentViewResponse::Redraw
                } else {
                    ContentViewResponse::Ignored
                }
            }
            WMEK::Move => {
                if hit != self.interaction.hovered {
                    self.interaction.hovered = hit;
                    ContentViewResponse::Redraw
                } else {
                    ContentViewResponse::Ignored
                }
            }
            WMEK::Press(MousePress::Left) => {
                self.interaction.pressed = hit;
                ContentViewResponse::Redraw
            }
            WMEK::Release(MousePress::Left) => {
                let pressed = self.interaction.pressed.take();
                if hit.is_some() && hit == pressed {
                    match hit.expect("checked as some") {
                        OverviewAction::CloseOverview => ContentViewResponse::Close,
                        OverviewAction::OpenThread(tab_id) => self
                            .card_keys
                            .get(&tab_id)
                            .cloned()
                            .map(|key| open_thread_response(key, self.owner_id))
                            .unwrap_or(ContentViewResponse::Redraw),
                        OverviewAction::CloseTab(tab_id) => self.request_close_tab(tab_id),
                        OverviewAction::ConfirmCloseTab => self.confirm_close_tab(),
                        OverviewAction::CancelCloseTab => self.cancel_close_tab(),
                    }
                } else {
                    ContentViewResponse::Redraw
                }
            }
            _ => ContentViewResponse::Ignored,
        }
    }
}

impl ContentView for LiveOverviewView {
    fn title(&self) -> String {
        crate::i18n::tr("live-overview-title")
    }

    fn show_in_tab_bar(&self) -> bool {
        false
    }

    fn presentation(&self) -> ContentViewPresentation {
        ContentViewPresentation::FullWindow
    }

    fn paint_surface_background(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        surface: RectF,
        _palette: UiPalette,
    ) -> anyhow::Result<()> {
        let colors = overview_colors(crate::native_settings::effective_appearance());
        ctx.draw_corner_gradient(
            layers,
            0,
            surface,
            colors.surface_top_left,
            colors.surface_top_right,
            colors.surface_bottom_left,
            colors.surface_bottom_right,
        )
    }

    fn typography(&self) -> ContentViewTypography {
        ContentViewTypography::Overview
    }

    fn tab_key(&self) -> Option<String> {
        Some(LIVE_OVERVIEW_CONTENT_VIEW_KEY.to_string())
    }

    fn on_reactivated(&mut self) -> ContentViewResponse {
        self.interaction = InteractionState::default();
        self.pending_close = None;
        self.scrollbar_visible_until = None;
        ContentViewResponse::Redraw
    }

    fn next_frame_time(&self) -> Option<Instant> {
        self.next_frame_deadline(Instant::now())
    }

    fn set_live_resizing(&mut self, live_resizing: bool) -> bool {
        if self.live_resizing == live_resizing {
            false
        } else {
            self.live_resizing = live_resizing;
            true
        }
    }

    fn terminal_landing_rect(&self, tab_id: TabId) -> Option<RectF> {
        self.card_preview_rects.get(&tab_id).copied()
    }

    fn set_terminal_in_flight(&mut self, tab_id: Option<TabId>) {
        self.terminal_in_flight = tab_id;
    }

    fn set_host_preview_aspect(&mut self, aspect: f32) {
        if aspect.is_finite() && aspect > 0.0 {
            self.host_preview_aspect = aspect.clamp(HOST_PREVIEW_ASPECT_MIN, HOST_PREVIEW_ASPECT_MAX);
        }
    }

    fn terminal_previews(&self) -> Vec<TerminalPreviewRequest> {
        self.previews.clone()
    }

    fn paint_after_terminal_previews(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        _title_font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        for chrome in &self.preview_chrome {
            ctx.draw_rounded_preview_chrome(
                layers,
                2,
                chrome.preview,
                chrome.clip,
                chrome.fill,
                chrome.border,
                ctx.px(PREVIEW_RADIUS),
            )?;
        }
        self.paint_scroll_masks(ctx, layers)?;
        self.paint_fixed_controls(ctx, layers, area, palette)?;
        if self
            .pending_close
            .as_ref()
            .is_some_and(|pending| Mux::get().get_tab(pending.tab_id).is_none())
        {
            self.pending_close = None;
            self.interaction = InteractionState::default();
        }
        self.paint_close_confirmation(ctx, layers, area, palette, font, section_font)
    }

    fn wants_pane_output(&self, pane_id: PaneId) -> bool {
        self.visible_panes.contains(&pane_id)
    }

    fn paint(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        _cursor_on: bool,
    ) -> anyhow::Result<()> {
        self.paint_impl(ctx, layers, area, palette, font, title_font, section_font)
    }

    fn on_mouse(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse {
        self.on_mouse_impl(x, y, kind)
    }

    fn on_wheel(&mut self, _x: f32, _y: f32, event: &window::MouseEvent) -> ContentViewResponse {
        if self.pending_close.is_some() {
            return ContentViewResponse::Ignored;
        }
        self.scroll
            .set_phase(event.momentum_phase.or(event.scroll_phase));

        let before = (self.scroll.offset, self.scroll.velocity);
        if let Some(delta) = precise_wheel_delta_pixels(event) {
            // A trackpad is already carrying its own momentum; take the
            // pixels it reports and add nothing.
            self.scroll.scroll_by(delta);
        } else if let WMEK::VertWheel(amount) = event.kind {
            // A notched wheel has no glide of its own, so give it one rather
            // than teleporting the list a fixed distance per click.
            self.scroll
                .scroll_by_smooth(wheel_delta_pixels(amount, self.last_ui_scale));
        } else {
            return ContentViewResponse::Ignored;
        }

        if (before.0 - self.scroll.offset).abs() > 0.01
            || (before.1 - self.scroll.velocity).abs() > 0.5
        {
            self.reveal_scrollbar(Instant::now());
            ContentViewResponse::Redraw
        } else {
            ContentViewResponse::Ignored
        }
    }

    fn on_key(&mut self, key: KeyCode, _mods: KeyModifiers) -> ContentViewResponse {
        if key == KeyCode::Escape {
            if self.pending_close.is_some() {
                self.cancel_close_tab()
            } else {
                ContentViewResponse::Close
            }
        } else {
            ContentViewResponse::Ignored
        }
    }
}

fn overview_colors(appearance: Appearance) -> OverviewColors {
    match appearance {
        Appearance::Light | Appearance::LightHighContrast => OverviewColors {
            surface_top_left: srgb(238, 238, 242, 255),
            surface_top_right: srgb(239, 240, 245, 255),
            surface_bottom_left: srgb(230, 232, 238, 255),
            surface_bottom_right: srgb(231, 234, 241, 255),
            card: srgb(255, 255, 255, 248),
            card_hover: srgb(250, 250, 252, 250),
            card_pressed: srgb(242, 242, 245, 252),
            shadow: srgb(0, 0, 0, 110),
            preview: srgb(229, 229, 232, 255),
            preview_border: srgb(45, 45, 52, 42),
            active_border: srgb(60, 60, 64, 96),
            modal_scrim: srgb(18, 18, 22, 72),
        },
        Appearance::Dark | Appearance::DarkHighContrast => OverviewColors {
            surface_top_left: srgb(25, 25, 26, 255),
            surface_top_right: srgb(26, 26, 29, 255),
            surface_bottom_left: srgb(20, 21, 24, 255),
            surface_bottom_right: srgb(21, 22, 27, 255),
            card: srgb(42, 42, 46, 248),
            card_hover: srgb(48, 48, 52, 250),
            card_pressed: srgb(54, 54, 58, 252),
            shadow: srgb(0, 0, 0, 148),
            preview: srgb(17, 17, 19, 255),
            preview_border: srgb(255, 255, 255, 28),
            active_border: srgb(205, 205, 210, 92),
            modal_scrim: srgb(0, 0, 0, 112),
        },
    }
}

fn srgb(red: u8, green: u8, blue: u8, alpha: u8) -> LinearRgba {
    LinearRgba::with_srgba(red, green, blue, alpha)
}

fn horizontal_page_pad(ctx: &DrawContext, area: RectF) -> f32 {
    ctx.px(PAGE_PAD_X).min(area.size.width * 0.075)
}

fn interpolate_color(from: LinearRgba, to: LinearRgba, amount: f32) -> LinearRgba {
    let amount = amount.clamp(0.0, 1.0);
    LinearRgba(
        from.0 + (to.0 - from.0) * amount,
        from.1 + (to.1 - from.1) * amount,
        from.2 + (to.2 - from.2) * amount,
        from.3 + (to.3 - from.3) * amount,
    )
}

#[allow(clippy::too_many_arguments)]
fn draw_surface_gradient_slice(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    surface: RectF,
    slice: RectF,
    colors: OverviewColors,
    top_alpha: f32,
    bottom_alpha: f32,
) -> anyhow::Result<()> {
    if slice.size.width <= 0.0 || slice.size.height <= 0.0 || surface.size.height <= 0.0 {
        return Ok(());
    }
    let row = |y: f32, top: LinearRgba, bottom: LinearRgba| {
        let amount = ((y - surface.min_y()) / surface.size.height).clamp(0.0, 1.0);
        interpolate_color(top, bottom, amount)
    };
    let top_left = row(
        slice.min_y(),
        colors.surface_top_left,
        colors.surface_bottom_left,
    )
    .mul_alpha(top_alpha);
    let top_right = row(
        slice.min_y(),
        colors.surface_top_right,
        colors.surface_bottom_right,
    )
    .mul_alpha(top_alpha);
    let bottom_left = row(
        slice.max_y(),
        colors.surface_top_left,
        colors.surface_bottom_left,
    )
    .mul_alpha(bottom_alpha);
    let bottom_right = row(
        slice.max_y(),
        colors.surface_top_right,
        colors.surface_bottom_right,
    )
    .mul_alpha(bottom_alpha);
    ctx.draw_corner_gradient(
        layers,
        2,
        slice,
        top_left,
        top_right,
        bottom_left,
        bottom_right,
    )
}

fn live_tab_for_workspace(workspace: &str) -> Option<TabId> {
    let mux = Mux::get();
    for window_id in mux.iter_windows_in_workspace(workspace) {
        let Some(tab) = mux.get_active_tab_for_window(window_id) else {
            continue;
        };
        if tab.iter_panes().is_empty() {
            continue;
        }
        return Some(tab.tab_id());
    }
    None
}

fn terminal_preview_fingerprint(tab_id: TabId) -> Option<TerminalPreviewFingerprint> {
    let tab = Mux::get().get_tab(tab_id)?;
    let tab_size = tab.get_size();
    if tab_size.cols == 0 || tab_size.rows == 0 {
        return None;
    }

    let splits = tab.iter_splits();
    let panes = tab
        .iter_panes()
        .into_iter()
        .map(|positioned| {
            let pane = positioned.pane;
            let palette = pane.palette_override().unwrap_or_else(|| pane.palette());
            TerminalPreviewPaneFingerprint {
                pane_id: pane.pane_id(),
                index: positioned.index,
                is_active: positioned.is_active,
                is_zoomed: positioned.is_zoomed,
                left: positioned.left,
                top: positioned.top,
                width: positioned.width,
                height: positioned.height,
                dimensions: pane.get_dimensions(),
                seqno: pane.get_current_seqno(),
                palette_identity: palette_identity(&palette),
                cursor: pane.get_cursor_position(),
            }
        })
        .collect::<Vec<_>>();
    if panes.is_empty() {
        None
    } else {
        Some(TerminalPreviewFingerprint {
            tab_id,
            tab_size,
            panes,
            splits,
        })
    }
}

/// Prune the running-label cache to the tabs that are still live.
///
/// Keyed on every card `collect_groups` walked, not on the cards that were
/// drawn. Retaining against the drawn set -- which is what `card_keys` holds --
/// discarded every offscreen card's entry on each frame as soon as there were
/// more cards than fit in the viewport, so the next frame asked the system
/// again for all of them and [`RUNNING_LABEL_REFRESH`] never held anything.
/// The cost lands on the render thread, in a lookup that spawns a thread when
/// its own cache has gone stale.
fn retain_running_labels(
    labels: &mut HashMap<TabId, (Instant, Option<String>)>,
    groups: &[LiveGroup],
) {
    let live_tabs: HashSet<TabId> = groups
        .iter()
        .flat_map(|group| group.cards.iter().map(|card| card.tab_id))
        .collect();
    labels.retain(|tab_id, _| live_tabs.contains(tab_id));
}

fn palette_identity(palette: &wezterm_term::color::ColorPalette) -> u64 {
    let mut hasher = DefaultHasher::new();
    palette.colors.0.hash(&mut hasher);
    palette.foreground.hash(&mut hasher);
    palette.background.hash(&mut hasher);
    palette.cursor_fg.hash(&mut hasher);
    palette.cursor_bg.hash(&mut hasher);
    palette.cursor_border.hash(&mut hasher);
    palette.selection_fg.hash(&mut hasher);
    palette.selection_bg.hash(&mut hasher);
    palette.scrollbar_thumb.hash(&mut hasher);
    palette.split.hash(&mut hasher);
    hasher.finish()
}

fn capture_terminal_snapshot(tab_id: TabId) -> Option<TerminalPreviewSnapshot> {
    let tab = Mux::get().get_tab(tab_id)?;
    let tab_size = tab.get_size();
    if tab_size.cols == 0 || tab_size.rows == 0 {
        return None;
    }

    let splits = tab.iter_splits();
    let mut snapshots = Vec::new();
    for positioned in tab.iter_panes() {
        let pane = positioned.pane;
        let dimensions = pane.get_dimensions();
        // A positioned pane is measured in the tab's root-cell grid, while
        // its terminal dimensions are measured in that pane's own cells.
        // Those differ when a pane has a local font scale. Clamping the latter
        // to the former discards the extra rows/columns of a smaller-font pane
        // and makes a larger-font pane look artificially short in a preview.
        let rows = dimensions.viewport_rows;
        let cols = dimensions.cols;
        let first_row = dimensions.physical_top;
        let (resolved_top, lines) = if rows == 0 || cols == 0 {
            (first_row, Vec::new())
        } else {
            pane.get_lines(first_row..first_row.saturating_add(rows as isize))
        };
        snapshots.push(TerminalPreviewPaneSnapshot {
            pane_id: pane.pane_id(),
            is_active: positioned.is_active,
            left: positioned.left,
            top: positioned.top,
            width: positioned.width,
            height: positioned.height,
            cols,
            rows,
            resolved_top,
            lines,
            box_pixel_height: positioned.pixel_height,
            dimensions,
            palette: pane.palette_override().unwrap_or_else(|| pane.palette()),
            cursor: pane.get_cursor_position(),
        });
    }
    if snapshots.is_empty() {
        None
    } else {
        Some(TerminalPreviewSnapshot {
            tab_size,
            panes: snapshots,
            splits,
        })
    }
}

fn resolve_snapshot<T, F>(
    cache: &mut HashMap<LiveThreadKey, CachedPreview<T>>,
    key: &LiveThreadKey,
    fingerprint: Option<TerminalPreviewFingerprint>,
    now: Instant,
    refresh_interval: Option<Duration>,
    capture: F,
) -> (Option<Arc<T>>, Option<Instant>)
where
    F: FnOnce() -> Option<T>,
{
    let Some(fingerprint) = fingerprint else {
        return (
            cache.get(key).map(|cached| Arc::clone(&cached.snapshot)),
            None,
        );
    };

    if let Some(cached) = cache.get(key) {
        if cached.fingerprint == fingerprint {
            return (Some(Arc::clone(&cached.snapshot)), None);
        }
        if let Some(refresh_interval) = refresh_interval {
            let refresh_due = cached.captured_at + refresh_interval;
            if now < refresh_due {
                return (Some(Arc::clone(&cached.snapshot)), Some(refresh_due));
            }
        }
    }

    if let Some(snapshot) = capture() {
        cache.insert(
            key.clone(),
            CachedPreview {
                fingerprint,
                snapshot: Arc::new(snapshot),
                captured_at: now,
            },
        );
    }
    (
        cache.get(key).map(|cached| Arc::clone(&cached.snapshot)),
        None,
    )
}

/// The command in the foreground of a tab's active pane, basename only, or
/// `None` when the pane is a shell waiting at its prompt.
///
/// `AllowStale` keeps this to a cache read: it runs for every card on every
/// frame, and a name that is one refresh out of date is not worth walking the
/// process table for.
fn foreground_process_name(tab_id: TabId) -> Option<String> {
    let pane = Mux::get().get_tab(tab_id)?.get_active_pane()?;
    let path = pane.get_foreground_process_name(CachePolicy::AllowStale)?;
    let name = std::path::Path::new(&path)
        .file_name()?
        .to_string_lossy()
        .to_string();
    if crate::termwindow::ui::is_default_shell_title(&name) {
        return None;
    }
    Some(name)
}

fn card_title(project_name: &str, thread_name: &str) -> String {
    if project_name.is_empty() || project_name == thread_name {
        thread_name.to_string()
    } else {
        format!("{project_name} · {thread_name}")
    }
}

fn close_tab_response(tab_id: TabId) -> ContentViewResponse {
    ContentViewResponse::Run(Box::new(move |term_window: &mut TermWindow| {
        Mux::get().remove_tab(tab_id);
        term_window.invalidate_window();
    }))
}

fn live_tab_for_key(key: &LiveThreadKey) -> Option<TabId> {
    let mux = Mux::get();
    let live_workspaces = mux.iter_workspaces();
    let state = workspace_threads::thread_connection_state(&key.thread_id, &live_workspaces)?;
    if state.space_id != key.space_id || !state.is_live {
        return None;
    }
    if let Some(domain_name) = workspace_threads::client_domain_for_space(&key.space_id) {
        if mux
            .get_domain_by_name(&domain_name)
            .is_none_or(|domain| domain.state() != DomainState::Attached)
        {
            return None;
        }
    }
    live_tab_for_workspace(&state.workspace_name)
}

fn open_thread_response(key: LiveThreadKey, source_owner_id: u64) -> ContentViewResponse {
    ContentViewResponse::Run(Box::new(move |term_window: &mut TermWindow| {
        if live_tab_for_key(&key).is_none() {
            term_window.invalidate_window();
            return;
        }

        let Some(source_window) = term_window.window.clone() else {
            return;
        };

        if let Some(target_owner_id) = workspace_threads::window_owner_for_space(&key.space_id) {
            if target_owner_id != source_owner_id {
                let Some(target) = crate::frontend::front_end()
                    .gui_window_for_recovery_slot(FrontendRecoverySlot::Window(target_owner_id))
                else {
                    term_window.invalidate_window();
                    return;
                };
                let target_window = target.window.clone();
                target.window.notify(TermWindowNotif::Apply(Box::new(
                    move |target_term_window| {
                        // Ownership and liveness can change between the click
                        // and this window's event-loop callback. Do not steal a
                        // Space or close the source overview on a stale lookup.
                        if workspace_threads::window_owner_for_space(&key.space_id)
                            != Some(target_owner_id)
                            || target_term_window.frontend_recovery_slot()
                                != FrontendRecoverySlot::Window(target_owner_id)
                            || live_tab_for_key(&key).is_none()
                        {
                            source_window.invalidate();
                            return;
                        }

                        target_term_window
                            .activate_workspace_thread(key.thread_id.clone(), &target_window);
                        target_window.focus();
                        source_window.notify(TermWindowNotif::Apply(Box::new(
                            |source_term_window| {
                                if source_term_window
                                    .active_content_view_key_is(LIVE_OVERVIEW_CONTENT_VIEW_KEY)
                                {
                                    source_term_window.close_content_view();
                                }
                            },
                        )));
                    },
                )));
                return;
            }
        }

        if term_window.switch_space_to_thread(key.space_id, Some(key.thread_id), &source_window) {
            // `switch_space_to_thread` deactivates content views as part of a
            // successful navigation. Remove this singleton so its next entry
            // captures a fresh host aspect and live snapshot set.
            term_window.close_content_view();
        }
    }))
}

fn max_columns_for_surface_width(surface_width: f32, five_column_width: f32) -> usize {
    if surface_width >= five_column_width {
        MAX_COLUMNS
    } else {
        MAX_COLUMNS_BELOW_WIDE_BREAKPOINT
    }
}

fn grid_columns(width: f32, minimum: f32, gap: f32, maximum: usize) -> usize {
    if maximum == 0 {
        return 0;
    }
    if maximum == 1 || width <= minimum {
        1
    } else {
        (((width + gap) / (minimum + gap)).floor() as usize).clamp(1, maximum)
    }
}

fn single_orphan_group_count(counts: &[usize], columns: usize) -> usize {
    if columns == 0 {
        return 0;
    }
    counts
        .iter()
        .filter(|&&count| count > columns && count % columns == 1)
        .count()
}

fn shared_grid_columns(
    counts: &[usize],
    content_width: f32,
    maximum_columns: usize,
    minimum: f32,
    orphan_comfort_width: f32,
    gap: f32,
) -> usize {
    let mut columns = grid_columns(content_width, minimum, gap, maximum_columns);
    if columns == 0 {
        return 0;
    }
    let initial_available =
        (content_width - gap * columns.saturating_sub(1) as f32) / columns as f32;
    if columns > 2 && initial_available < orphan_comfort_width {
        let reduced_columns = columns - 1;
        if single_orphan_group_count(counts, reduced_columns)
            < single_orphan_group_count(counts, columns)
        {
            columns = reduced_columns;
        }
    }
    columns
}

#[allow(clippy::too_many_arguments)]
fn group_grid(
    count: usize,
    content_width: f32,
    columns: usize,
    maximum: f32,
    gap: f32,
    card_header_height: f32,
    card_inset: f32,
    preview_aspect: f32,
) -> GroupGrid {
    if columns == 0 {
        return GroupGrid {
            columns: 0,
            rows: 0,
            card_width: 0.0,
            card_height: 0.0,
        };
    }
    let available = (content_width - gap * columns.saturating_sub(1) as f32) / columns as f32;
    let card_width = available.min(maximum).max(1.0);
    let preview_width = (card_width - card_inset * 2.0).max(1.0);
    GroupGrid {
        columns,
        rows: (count + columns - 1) / columns,
        card_width,
        card_height: card_header_height + preview_width / preview_aspect + card_inset,
    }
}

#[allow(clippy::too_many_arguments)]
fn group_layouts(
    counts: &[usize],
    content_width: f32,
    maximum_columns: usize,
    minimum: f32,
    orphan_comfort_width: f32,
    maximum: f32,
    card_gap: f32,
    header_height: f32,
    title_card_gap: f32,
    card_header_height: f32,
    card_inset: f32,
    preview_aspect: f32,
    group_gap: f32,
) -> (Vec<GroupLayout>, f32) {
    let mut layouts = Vec::with_capacity(counts.len());
    let mut y = 0.0;
    let columns = shared_grid_columns(
        counts,
        content_width,
        maximum_columns,
        minimum,
        orphan_comfort_width,
        card_gap,
    );
    for &count in counts {
        let grid = group_grid(
            count,
            content_width,
            columns,
            maximum,
            card_gap,
            card_header_height,
            card_inset,
            preview_aspect,
        );
        let cards_y = y + header_height + if count > 0 { title_card_gap } else { 0.0 };
        layouts.push(GroupLayout {
            header_y: y,
            cards_y,
            grid,
        });
        y = cards_y;
        if grid.rows > 0 {
            y +=
                grid.rows as f32 * grid.card_height + grid.rows.saturating_sub(1) as f32 * card_gap;
        }
        y += group_gap;
    }
    (layouts, (y - group_gap).max(0.0))
}

fn card_rect(
    index: usize,
    count: usize,
    grid: GroupGrid,
    content_x: f32,
    content_width: f32,
    cards_y: f32,
    gap: f32,
) -> RectF {
    let row = index / grid.columns;
    let column = index % grid.columns;
    let row_start = row * grid.columns;
    let row_count = (count - row_start).min(grid.columns);
    let row_width = row_count as f32 * grid.card_width + row_count.saturating_sub(1) as f32 * gap;
    let row_x = content_x + (content_width - row_width).max(0.0) / 2.0;
    euclid::rect(
        row_x + column as f32 * (grid.card_width + gap),
        cards_y + row as f32 * (grid.card_height + gap),
        grid.card_width,
        grid.card_height,
    )
}

fn row_visible(y: f32, height: f32, viewport: RectF) -> bool {
    y + height > viewport.min_y() && y < viewport.max_y()
}

fn card_is_warm(rect: RectF, viewport: RectF, overscan: f32) -> bool {
    let warm_top = viewport.min_y() - overscan.max(0.0);
    let warm_bottom = viewport.max_y() + overscan.max(0.0);
    rect.max_y() > warm_top && rect.min_y() < warm_bottom
}

fn row_fully_visible(y: f32, height: f32, viewport: RectF) -> bool {
    y >= viewport.min_y() && y + height <= viewport.max_y()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn test_group_grid(
        count: usize,
        content_width: f32,
        maximum_columns: usize,
        minimum: f32,
        orphan_comfort_width: f32,
        maximum: f32,
        gap: f32,
        card_header_height: f32,
        card_inset: f32,
        preview_aspect: f32,
    ) -> GroupGrid {
        let columns = shared_grid_columns(
            &[count],
            content_width,
            maximum_columns,
            minimum,
            orphan_comfort_width,
            gap,
        );
        group_grid(
            count,
            content_width,
            columns,
            maximum,
            gap,
            card_header_height,
            card_inset,
            preview_aspect,
        )
    }

    #[test]
    fn five_columns_begin_at_1500_logical_pixels_at_every_dpi() {
        for scale in [0.5_f32, 1.0, 1.5, 2.0] {
            let threshold = FIVE_COLUMN_WINDOW_WIDTH * scale;
            assert_eq!(max_columns_for_surface_width(threshold, threshold), 5);
            assert_eq!(max_columns_for_surface_width(threshold - 1.0, threshold), 4);

            let content_width = threshold - PAGE_PAD_X * scale * 2.0;
            let wide = test_group_grid(
                5,
                content_width,
                5,
                CARD_MIN_WIDTH * scale,
                CARD_ORPHAN_COMFORT_WIDTH * scale,
                CARD_MAX_WIDTH * scale,
                CARD_GAP * scale,
                CARD_HEADER_HEIGHT * scale,
                CARD_INSET * scale,
                1.8,
            );
            let narrow = test_group_grid(
                5,
                content_width - 1.0,
                4,
                CARD_MIN_WIDTH * scale,
                CARD_ORPHAN_COMFORT_WIDTH * scale,
                CARD_MAX_WIDTH * scale,
                CARD_GAP * scale,
                CARD_HEADER_HEIGHT * scale,
                CARD_INSET * scale,
                1.8,
            );
            assert_eq!((wide.columns, wide.rows), (5, 1));
            assert_eq!((narrow.columns, narrow.rows), (4, 2));
        }
    }

    #[test]
    fn retina_window_from_review_wraps_the_fifth_card() {
        assert_eq!(max_columns_for_surface_width(2202.0, 3000.0), 4);
        assert_eq!(max_columns_for_surface_width(3000.0, 3000.0), 5);
    }

    #[test]
    fn compressed_five_card_window_balances_as_three_plus_two() {
        let surface_width = 1616.0;
        let content_width = surface_width - PAGE_PAD_X * 2.0;
        let grid = test_group_grid(
            5,
            content_width,
            max_columns_for_surface_width(surface_width, FIVE_COLUMN_WINDOW_WIDTH),
            CARD_MIN_WIDTH,
            CARD_ORPHAN_COMFORT_WIDTH,
            CARD_MAX_WIDTH,
            CARD_GAP,
            CARD_HEADER_HEIGHT,
            CARD_INSET,
            1.8,
        );
        assert_eq!((grid.columns, grid.rows), (3, 2));
    }

    #[test]
    fn roomy_sub_1500_window_keeps_four_plus_one() {
        let surface_width = 2600.0;
        let content_width = surface_width - PAGE_PAD_X * 2.0;
        let grid = test_group_grid(
            5,
            content_width,
            max_columns_for_surface_width(surface_width, FIVE_COLUMN_WINDOW_WIDTH),
            CARD_MIN_WIDTH,
            CARD_ORPHAN_COMFORT_WIDTH,
            CARD_MAX_WIDTH,
            CARD_GAP,
            CARD_HEADER_HEIGHT,
            CARD_INSET,
            1.8,
        );
        assert_eq!((grid.columns, grid.rows), (4, 2));
    }

    #[test]
    fn responsive_grid_falls_back_through_four_three_two_one() {
        assert_eq!(grid_columns(1648.0, 400.0, 16.0, 4), 4);
        assert_eq!(grid_columns(1232.0, 400.0, 16.0, 4), 3);
        assert_eq!(grid_columns(816.0, 400.0, 16.0, 4), 2);
        assert_eq!(grid_columns(400.0, 400.0, 16.0, 4), 1);
    }

    #[test]
    fn one_to_ten_cards_form_complete_non_overlapping_rows() {
        for count in 1..=10 {
            let grid = test_group_grid(count, 1332.0, 5, 400.0, 480.0, 640.0, 16.0, 44.0, 8.0, 1.8);
            let rects = (0..count)
                .map(|idx| card_rect(idx, count, grid, 84.0, 1332.0, 100.0, 16.0))
                .collect::<Vec<_>>();
            assert_eq!(grid.rows, (count + grid.columns - 1) / grid.columns);
            for (idx, rect) in rects.iter().enumerate() {
                assert!(rect.min_x() >= 84.0);
                assert!(rect.max_x() <= 84.0 + 1332.0 + 0.01);
                for other in rects.iter().skip(idx + 1) {
                    assert!(rect.intersection(other).is_none());
                }
            }
        }
    }

    #[test]
    fn groups_with_different_counts_share_one_card_size() {
        let (layouts, _) = group_layouts(
            &[3, 4],
            2464.0,
            4,
            400.0,
            480.0,
            640.0,
            16.0,
            42.0,
            8.0,
            44.0,
            8.0,
            1.8,
            48.0,
        );
        assert_eq!(layouts[0].grid.columns, 4);
        assert_eq!(layouts[1].grid.columns, 4);
        assert_eq!(layouts[0].grid.card_width, layouts[1].grid.card_width);
        assert_eq!(layouts[0].grid.card_height, layouts[1].grid.card_height);
    }

    #[test]
    fn orphan_rebalance_is_shared_by_every_group() {
        let (layouts, _) = group_layouts(
            &[3, 5],
            1800.0,
            4,
            400.0,
            480.0,
            640.0,
            16.0,
            42.0,
            8.0,
            44.0,
            8.0,
            1.8,
            48.0,
        );
        assert_eq!(layouts[0].grid.columns, 3);
        assert_eq!(layouts[1].grid.columns, 3);
        assert_eq!(layouts[0].grid.card_width, layouts[1].grid.card_width);
    }

    #[test]
    fn every_card_uses_the_same_host_preview_aspect() {
        let aspect = 1.73;
        for count in [1, 3, 5, 8] {
            let grid = test_group_grid(
                count, 1332.0, 5, 400.0, 480.0, 640.0, 16.0, 44.0, 8.0, aspect,
            );
            let preview_width = grid.card_width - 16.0;
            let preview_height = grid.card_height - 44.0 - 8.0;
            assert!((preview_width / preview_height - aspect).abs() < 0.001);
        }
    }

    #[test]
    fn card_hit_rect_matches_painted_geometry() {
        let grid = test_group_grid(5, 1332.0, 5, 400.0, 480.0, 640.0, 16.0, 44.0, 8.0, 1.8);
        let rect = card_rect(2, 5, grid, 84.0, 1332.0, 100.0, 16.0);
        let mut widgets = UiContext::default();
        widgets.push(rect, WidgetKind::SidebarRow, OverviewAction::OpenThread(42));
        let hit = widgets
            .hit_test(rect.center().x, rect.center().y)
            .expect("card center should hit");
        assert_eq!(hit.action, OverviewAction::OpenThread(42));
    }

    #[test]
    fn card_close_hit_target_wins_over_card_open_target() {
        let card = euclid::rect(10.0, 20.0, 320.0, 180.0);
        let close = euclid::rect(294.0, 26.0, 28.0, 28.0);
        let mut widgets = UiContext::default();
        widgets.push(card, WidgetKind::SidebarRow, OverviewAction::OpenThread(42));
        widgets.push(close, WidgetKind::Button, OverviewAction::CloseTab(42));
        let hit = widgets
            .hit_test(close.center().x, close.center().y)
            .expect("close button should hit");
        assert_eq!(hit.action, OverviewAction::CloseTab(42));
    }

    #[test]
    fn pending_close_can_be_cancelled_or_confirmed_without_closing_overview() {
        let mut view = LiveOverviewView::new(0, "space", "workspace", 1.8);
        view.pending_close = Some(PendingClose {
            tab_id: 42,
            title: "main".to_string(),
        });
        assert!(matches!(
            view.cancel_close_tab(),
            ContentViewResponse::Redraw
        ));
        assert!(view.pending_close.is_none());

        view.pending_close = Some(PendingClose {
            tab_id: 42,
            title: "main".to_string(),
        });
        assert!(matches!(
            view.confirm_close_tab(),
            ContentViewResponse::Run(_)
        ));
        assert!(view.pending_close.is_none());
    }

    fn live_group_with_active(count: usize, active: usize) -> LiveGroup {
        LiveGroup {
            name: "space".to_string(),
            offline: false,
            cards: (0..count)
                .map(|index| LiveCard {
                    key: LiveThreadKey {
                        space_id: "space".to_string(),
                        thread_id: format!("thread-{index}"),
                    },
                    title: format!("thread {index}"),
                    running: None,
                    tab_id: index as TabId,
                    active: index == active,
                })
                .collect(),
        }
    }

    fn reveal_layout(columns: usize, rows: usize, card_height: f32) -> Vec<GroupLayout> {
        vec![GroupLayout {
            header_y: 0.0,
            cards_y: 40.0,
            grid: GroupGrid {
                columns,
                rows,
                card_width: 480.0,
                card_height,
            },
        }]
    }

    #[test]
    fn opening_scrolls_an_active_card_below_the_fold_into_view() {
        let mut view = LiveOverviewView::new(0, "space", "workspace", 1.8);
        view.viewport = euclid::rect(0.0, 0.0, 1000.0, 400.0);
        let layouts = reveal_layout(2, 3, 300.0);
        let groups = vec![live_group_with_active(6, 5)];
        // Three rows of 300 separated by a 16 gap, below a 40px heading.
        view.scroll.set_extents(400.0, 40.0 + 3.0 * 300.0 + 2.0 * 16.0);

        let offset = view
            .offset_revealing_active(&groups, &layouts, 0.0, 1000.0, 16.0)
            .expect("the last row starts below the fold");
        // The last row wants to be centred at 622; the list bottoms out at 572.
        assert!((offset - 572.0).abs() < 0.01, "offset was {offset}");
    }

    #[test]
    fn a_revealed_card_is_centred_when_the_list_is_long_enough() {
        let mut view = LiveOverviewView::new(0, "space", "workspace", 1.8);
        view.viewport = euclid::rect(0.0, 0.0, 1000.0, 400.0);
        let layouts = reveal_layout(1, 10, 100.0);
        let groups = vec![live_group_with_active(10, 4)];
        view.scroll.set_extents(400.0, 40.0 + 10.0 * 100.0 + 9.0 * 16.0);

        let offset = view
            .offset_revealing_active(&groups, &layouts, 0.0, 1000.0, 16.0)
            .expect("the fifth row is below the fold");
        // Row 4 sits at 40 + 4 * 116 = 504; centring a 100 tall card in 400
        // lifts it by 150.
        assert!((offset - 354.0).abs() < 0.01, "offset was {offset}");
    }

    #[test]
    fn opening_leaves_an_already_visible_active_card_alone() {
        let mut view = LiveOverviewView::new(0, "space", "workspace", 1.8);
        view.viewport = euclid::rect(0.0, 0.0, 1000.0, 400.0);
        let layouts = reveal_layout(2, 3, 300.0);
        let groups = vec![live_group_with_active(6, 0)];
        view.scroll.set_extents(400.0, 40.0 + 3.0 * 300.0 + 2.0 * 16.0);

        assert!(view
            .offset_revealing_active(&groups, &layouts, 0.0, 1000.0, 16.0)
            .is_none());
    }

    #[test]
    fn a_list_with_no_active_card_is_left_where_it_is() {
        let mut view = LiveOverviewView::new(0, "space", "workspace", 1.8);
        view.viewport = euclid::rect(0.0, 0.0, 1000.0, 400.0);
        let layouts = reveal_layout(2, 3, 300.0);
        let mut groups = vec![live_group_with_active(6, 0)];
        groups[0].cards[0].active = false;
        view.scroll.set_extents(400.0, 40.0 + 3.0 * 300.0 + 2.0 * 16.0);

        assert!(view
            .offset_revealing_active(&groups, &layouts, 0.0, 1000.0, 16.0)
            .is_none());
    }

    #[test]
    fn offscreen_cards_keep_their_running_label_between_frames() {
        // Six cards; a viewport that fits two. Whether a card was drawn is not
        // this function's business -- every live tab keeps its entry, or the
        // one-second cache never survives a frame.
        let groups = vec![live_group_with_active(6, 0)];
        let now = Instant::now();
        let mut labels: HashMap<TabId, (Instant, Option<String>)> = (0..6)
            .map(|index| (index as TabId, (now, Some(format!("cmd-{index}")))))
            .collect();

        retain_running_labels(&mut labels, &groups);

        assert_eq!(labels.len(), 6, "every live tab keeps its label");
        assert_eq!(
            labels[&5].1.as_deref(),
            Some("cmd-5"),
            "the last row is offscreen but still live"
        );
    }

    #[test]
    fn a_closed_tab_loses_its_running_label() {
        let groups = vec![live_group_with_active(2, 0)];
        let now = Instant::now();
        let mut labels: HashMap<TabId, (Instant, Option<String>)> = (0..4)
            .map(|index| (index as TabId, (now, Some("cmd".to_string()))))
            .collect();

        retain_running_labels(&mut labels, &groups);

        assert_eq!(labels.len(), 2);
        assert!(labels.contains_key(&0) && labels.contains_key(&1));
    }

    fn card_key() -> LiveThreadKey {
        LiveThreadKey {
            space_id: "space".to_string(),
            thread_id: "thread".to_string(),
        }
    }

    /// One frame of `paint_impl`: every travel is advanced once, then each
    /// card asks where it should be drawn.
    fn card_frame(
        view: &mut LiveOverviewView,
        key: &LiveThreadKey,
        target: RectF,
        now: Instant,
    ) -> RectF {
        for motion in view.card_motion.values_mut() {
            motion.travel.advance(now);
        }
        view.settle_card_rect(key, target, now)
    }

    #[test]
    fn a_card_appears_where_the_grid_puts_it_and_stays_there() {
        let mut view = LiveOverviewView::new(0, "space", "workspace", 1.8);
        let key = card_key();
        let now = Instant::now();
        let slot = euclid::rect(0.0, 0.0, 100.0, 80.0);

        assert_eq!(card_frame(&mut view, &key, slot, now), slot);
        let later = now + Duration::from_millis(500);
        assert_eq!(card_frame(&mut view, &key, slot, later), slot);
        assert!(!view.card_motion[&key].travel.is_running());
    }

    #[test]
    fn a_card_whose_neighbour_closed_travels_into_the_gap() {
        let mut view = LiveOverviewView::new(0, "space", "workspace", 1.8);
        let key = card_key();
        let start = Instant::now();
        let was = euclid::rect(0.0, 200.0, 100.0, 80.0);
        let now_at = euclid::rect(0.0, 0.0, 100.0, 80.0);
        let at = |ms: u64| start + Duration::from_millis(ms);

        card_frame(&mut view, &key, was, at(0));

        // The frame that discovers the new layout is also the frame that pays
        // for redrawing it, so the card has not set off yet.
        assert_eq!(card_frame(&mut view, &key, now_at, at(10)), was);
        assert_eq!(card_frame(&mut view, &key, now_at, at(20)), was);

        // Halfway through, ease-out has it well past the midpoint.
        let midway = card_frame(&mut view, &key, now_at, at(110));
        assert!(midway.origin.y < 100.0 && midway.origin.y > 0.0);

        assert_eq!(card_frame(&mut view, &key, now_at, at(400)), now_at);
        assert!(!view.card_motion[&key].travel.is_running());
    }

    #[test]
    fn closing_a_second_card_mid_travel_does_not_send_the_first_backwards() {
        let mut view = LiveOverviewView::new(0, "space", "workspace", 1.8);
        let key = card_key();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let bottom = euclid::rect(0.0, 200.0, 100.0, 80.0);
        let middle = euclid::rect(0.0, 100.0, 100.0, 80.0);
        let top = euclid::rect(0.0, 0.0, 100.0, 80.0);

        card_frame(&mut view, &key, bottom, at(0));
        card_frame(&mut view, &key, middle, at(10));
        card_frame(&mut view, &key, middle, at(20));
        let mid_travel = card_frame(&mut view, &key, middle, at(110));
        assert!(mid_travel.origin.y < 200.0 && mid_travel.origin.y > 100.0);

        // A second card closes while this one is still moving.
        let redirected = card_frame(&mut view, &key, top, at(110));
        assert_eq!(redirected.origin.y, mid_travel.origin.y);

        card_frame(&mut view, &key, top, at(126));
        assert_eq!(card_frame(&mut view, &key, top, at(500)), top);
    }

    #[test]
    fn dragging_the_window_relays_out_the_grid_without_animating_it() {
        let mut view = LiveOverviewView::new(0, "space", "workspace", 1.8);
        let key = card_key();
        let start = Instant::now();
        let narrow = euclid::rect(0.0, 0.0, 100.0, 80.0);
        let wide = euclid::rect(0.0, 0.0, 260.0, 80.0);

        card_frame(&mut view, &key, narrow, start);
        view.live_resizing = true;
        // Chasing the window edge one frame behind reads as lag, not motion.
        assert_eq!(
            card_frame(&mut view, &key, wide, start + Duration::from_millis(8)),
            wide
        );
        assert!(!view.card_motion[&key].travel.is_running());
    }

    #[test]
    fn scrollbar_is_transient_and_schedules_its_hide_frame() {
        let mut view = LiveOverviewView::new(0, "space", "workspace", 1.8);
        view.scroll.set_extents(100.0, 300.0);
        let now = Instant::now();
        assert_eq!(view.scrollbar_opacity_at(now), 0.0);

        view.reveal_scrollbar(now);
        let deadline = now + SCROLLBAR_VISIBLE_INTERVAL;
        assert!(view.scrollbar_opacity_at(deadline - Duration::from_millis(1)) > 0.0);

        // Full strength until the fade starts, then on its way out.
        let fade_from = deadline - SCROLLBAR_FADE;
        assert_eq!(view.scrollbar_opacity_at(fade_from), 1.0);
        let half_faded = view.scrollbar_opacity_at(fade_from + SCROLLBAR_FADE / 2);
        assert!(half_faded > 0.0 && half_faded < 1.0);
        assert_eq!(view.scrollbar_opacity_at(deadline), 0.0);

        // One frame to begin the fade; once inside it, every frame.
        assert_eq!(view.next_frame_deadline(now), Some(fade_from));
        let mid_fade = fade_from + Duration::from_millis(20);
        assert_eq!(view.next_frame_deadline(mid_fade), Some(mid_fade));
        assert_eq!(view.next_frame_deadline(deadline), None);
    }

    #[test]
    fn snapshot_cache_reuses_unchanged_content_and_throttles_live_resize() {
        let key = LiveThreadKey {
            space_id: "local".to_string(),
            thread_id: "thread".to_string(),
        };
        let mut cache = HashMap::new();
        let now = Instant::now();
        let first_fingerprint = test_fingerprint(1);
        let (snapshot, due) = resolve_snapshot(
            &mut cache,
            &key,
            Some(first_fingerprint.clone()),
            now,
            None,
            || Some(7_u8),
        );
        assert_eq!(*snapshot.unwrap(), 7);
        assert!(due.is_none());

        let (snapshot, due) = resolve_snapshot(
            &mut cache,
            &key,
            Some(first_fingerprint),
            now + Duration::from_millis(1),
            None,
            || panic!("unchanged fingerprint must not recapture"),
        );
        assert_eq!(*snapshot.unwrap(), 7);
        assert!(due.is_none());

        let changed_fingerprint = test_fingerprint(2);
        let (snapshot, due) = resolve_snapshot(
            &mut cache,
            &key,
            Some(changed_fingerprint.clone()),
            now + Duration::from_millis(10),
            Some(LIVE_RESIZE_PREVIEW_INTERVAL),
            || panic!("live resize must reuse until the refresh deadline"),
        );
        assert_eq!(*snapshot.unwrap(), 7);
        assert_eq!(due, Some(now + LIVE_RESIZE_PREVIEW_INTERVAL));

        let (snapshot, due) = resolve_snapshot(
            &mut cache,
            &key,
            Some(changed_fingerprint),
            now + Duration::from_millis(34),
            Some(LIVE_RESIZE_PREVIEW_INTERVAL),
            || Some(8_u8),
        );
        assert_eq!(*snapshot.unwrap(), 8);
        assert!(due.is_none());

        let (snapshot, _) = resolve_snapshot(
            &mut cache,
            &key,
            None,
            now + Duration::from_millis(35),
            None,
            || panic!("missing live tab keeps the last successful snapshot"),
        );
        assert_eq!(*snapshot.unwrap(), 8);
        let live = HashSet::<LiveThreadKey>::new();
        cache.retain(|cached, _| live.contains(cached));
        assert!(cache.is_empty());
    }

    #[test]
    fn overview_warms_only_visible_and_adjacent_rows() {
        let viewport = euclid::rect(0.0, 110.0, 1000.0, 100.0);
        let card_height = 100.0;
        let gap = 10.0;
        let warm = (0..100)
            .filter(|index| {
                let row = index / 5;
                let rect = euclid::rect(
                    (index % 5) as f32 * 100.0,
                    row as f32 * (card_height + gap),
                    90.0,
                    card_height,
                );
                card_is_warm(rect, viewport, card_height + gap)
            })
            .count();
        assert_eq!(warm, 15, "five visible cards plus one row on each side");
    }

    #[test]
    fn group_layout_accounts_for_wrapped_rows_and_offline_heading() {
        let (layouts, height) = group_layouts(
            &[8, 0],
            1332.0,
            5,
            400.0,
            480.0,
            640.0,
            16.0,
            42.0,
            10.0,
            44.0,
            8.0,
            1.8,
            48.0,
        );
        assert_eq!(layouts.len(), 2);
        assert!(layouts[1].header_y > layouts[0].cards_y);
        assert_eq!(height, layouts[1].cards_y);
    }

    fn test_fingerprint(seqno: usize) -> TerminalPreviewFingerprint {
        TerminalPreviewFingerprint {
            tab_id: 1,
            tab_size: TerminalSize::default(),
            panes: vec![TerminalPreviewPaneFingerprint {
                pane_id: 1,
                index: 0,
                is_active: true,
                is_zoomed: false,
                left: 0,
                top: 0,
                width: 80,
                height: 24,
                dimensions: RenderableDimensions::default(),
                seqno,
                palette_identity: 1,
                cursor: StableCursorPosition::default(),
            }],
            splits: Vec::new(),
        }
    }

    #[test]
    fn scoped_keys_do_not_collide_across_spaces() {
        let first = LiveThreadKey {
            space_id: "local".to_string(),
            thread_id: "thread-1".to_string(),
        };
        let second = LiveThreadKey {
            space_id: "server".to_string(),
            thread_id: "thread-1".to_string(),
        };
        assert_ne!(first, second);
    }

    #[test]
    fn card_title_keeps_project_and_thread_on_one_line() {
        assert_eq!(card_title("ThinkTerm", "main"), "ThinkTerm · main");
        assert_eq!(card_title("main", "main"), "main");
    }
}
