//! A thread in a window of its own.
//!
//! Every thread already runs in a mux window of its own, and a GUI window
//! shows one mux window at a time; the threads not on screen go on running
//! behind it. So nothing moves when a thread gets its own window: a new GUI
//! window, with no sidebars and no Space of its own, shows the thread's mux
//! window, and closing that window leaves the thread running unseen again.
//!
//! Two GUI windows cannot share a mux window, each resizing its terminals to
//! fit itself. A window that was showing the thread has to show something
//! else, so it goes to a page saying where the thread went, over an empty mux
//! window kept for the page alone. No thread owns that window and nothing
//! snapshots it (`is_thread_page_workspace`). Without a tab it has no pane to
//! run key bindings against, so the page brings an inert one.
//!
//! A row dragged out of the sidebar becomes a card, and goes back to being
//! the row if it comes back. No window draws past its own edges, so the card
//! is a little window of its own from the moment it sets out, following the
//! pointer wherever on the screen it goes. That window is Liquid Glass where
//! the system has it, with a surface on this window's GPU device showing the
//! whole card as rendered here. Growing is the window opening up over a
//! picture that stays where it is. It must never take the focus: a window
//! that loses it drops the drag.

use crate::frontend::front_end;
use crate::termwindow::content_view::{
    ContentView, ContentViewId, ContentViewResponse, TerminalPreviewRequest,
    TerminalPreviewSnapshot,
};
use crate::quad::{HeapQuadAllocator, QuadClipRect, TripleLayerQuadAllocator};
use crate::termwindow::render::draw::show_card_on_surface;
use crate::termwindow::render::paint::{CardComposite, PendingCardRender};
use crate::termwindow::webgpu::{CardRenderTexture, RawHandlePair};
use crate::termwindow::{domain_is_attached, get_window_class, TermWindow, TermWindowNotif};
use crate::ui::{
    rect, ControlState, DrawContext, InteractionState, UiContext, UiPalette,
    WidgetKind,
};
use crate::workspace_threads::{self, WorkspaceThreadId};
use anyhow::Context as _;
use config::keyassignment::KeyAssignment;
use config::window::WindowLevel;
use config::{Dimension, MacosWindowGlass, TermConfig};
use mux::pane::Pane;
use mux::tab::TabId;
use mux::window::WindowId as MuxWindowId;
use mux::Mux;
use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wezterm_font::LoadedFont;
use wezterm_term::{KeyCode, KeyModifiers};
use window::{
    Dimensions, FloatingCard, MouseEventKind as WMEK, MousePress, RectF, RequestedWindowGeometry, ScreenPoint,
    Window, WindowDecorations, WindowOps, WindowState,
};

/// Keeps the page's mux window alive with no tab in it, whichever view is in
/// front of the page at the moment.
const THREAD_PAGE_SURFACE: &str = "thinkterm-thread-elsewhere";

const THREAD_PAGE_VIEW_KEY_PREFIX: &str = "thread-elsewhere:";

const HERO_ICON_SIZE: f32 = 76.0;
const BUTTON_MARGIN: f32 = 6.0;
const BUTTON_PADDING: f32 = 16.0;
const PAD: f32 = 32.0;

/// The card a thread row becomes as it leaves the sidebar.
const TEAR_OFF_CARD_WIDTH: f32 = 424.0;
const TEAR_OFF_CARD_RADIUS: f32 = 20.0;
const TEAR_OFF_CARD_PADDING: f32 = 8.0;
/// Round the same centres as the card's corners, whatever those are.
const TEAR_OFF_THUMBNAIL_RADIUS: f32 = TEAR_OFF_CARD_RADIUS - TEAR_OFF_CARD_PADDING;
const TEAR_OFF_GROW: Duration = Duration::from_millis(220);
/// How long the card this window draws takes to give way to its own window.
const TEAR_OFF_HANDOVER: Duration = Duration::from_millis(100);
/// Room around the card in its own window for the shadow to fall in.
const TEAR_OFF_CARD_MARGIN: f32 = 56.0;

/// A page standing in for a thread that is open in a window of its own.
pub(crate) struct ThreadElsewherePage {
    pub(crate) thread_id: WorkspaceThreadId,
    pub(crate) mux_window_id: MuxWindowId,
    view_id: ContentViewId,
    /// What key bindings run against while there is no terminal here.
    key_pane: Option<Arc<dyn Pane>>,
}

/// A thread on its way to a window of its own, once it is running.
pub(crate) struct ThreadBoundForOwnWindow {
    thread_id: WorkspaceThreadId,
    position: Option<ScreenPoint>,
    /// What this window was showing before it opened the thread to get it
    /// running, to go back to.
    return_to: Option<WorkspaceThreadId>,
}

/// A thread row dragged out of the sidebar.
pub(crate) struct ThreadTearOff {
    started: Instant,
    /// The thread's terminal as it stood, for a thread that is running.
    preview: Option<(TabId, Arc<TerminalPreviewSnapshot>)>,
    /// Bound for the card, or back for the row.
    out: bool,
    /// When it set off that way, and how far out it was then.
    turned: Instant,
    turned_from: f32,
    place: TearOffPlace,
}

impl ThreadTearOff {
    fn new(preview: Option<(TabId, Arc<TerminalPreviewSnapshot>)>, now: Instant) -> Self {
        Self {
            started: now,
            preview,
            out: true,
            turned: now,
            turned_from: 0.0,
            place: TearOffPlace::Growing,
        }
    }

    pub(crate) fn is_out(&self) -> bool {
        self.out
    }

    fn aim(&mut self, out: bool, now: Instant) {
        if out != self.out {
            self.turned_from = self.grown(now);
            self.turned = now;
            self.out = out;
        }
    }

    fn travelled(&self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.turned);
        (elapsed.as_secs_f32() / TEAR_OFF_GROW.as_secs_f32()).min(1.0)
    }

    fn settled(&self, now: Instant) -> bool {
        self.travelled(now) >= 1.0
    }

    /// How far from the row it is, 0 to 1.
    fn grown(&self, now: Instant) -> f32 {
        let target = if self.out { 1.0 } else { 0.0 };
        let eased = crate::ui::anim::Easing::OutCubic.apply(self.travelled(now));
        self.turned_from + (target - self.turned_from) * eased
    }

    fn into_card_window(self) -> Option<ThreadCardWindow> {
        match self.place {
            TearOffPlace::Out { card, .. } => Some(card),
            _ => None,
        }
    }
}

/// Where a card is being drawn.
enum TearOffPlace {
    /// Here, with its own window not asked for yet.
    Growing,
    /// Here for good.
    Here,
    /// Here, with its own window on the way.
    Leaving {
        chrome: Rc<CardRenderTexture>,
        dimensions: Dimensions,
    },
    /// In its own window, since `since`.
    Out {
        card: ThreadCardWindow,
        since: Instant,
    },
}

/// The little window a card moves into, gone when this is.
pub(crate) struct ThreadCardWindow {
    window: Window,
    surface: wgpu::Surface<'static>,
    _handle: RawHandlePair,
    /// The card's surface and title, as much of it as the window shows.
    chrome: Rc<CardRenderTexture>,
    /// The whole card's size, which the surface keeps as the window grows.
    dimensions: Dimensions,
    /// The size the window was last given.
    size: Cell<(usize, usize)>,
    has_thumbnail: bool,
}

impl Drop for ThreadCardWindow {
    fn drop(&mut self) {
        self.window.close();
    }
}

/// Where the card goes `grown` of the way (0 to 1) from the row it comes
/// from: over the pointer, held about its title.
fn tear_off_card_rect(row: RectF, pointer: (f32, f32), size: (f32, f32), grown: f32) -> RectF {
    let width = row.size.width + (size.0 - row.size.width) * grown;
    let height = row.size.height + (size.1 - row.size.height) * grown;
    let held_x = (pointer.0 - row.origin.x) / row.size.width.max(1.0);
    let held_y = (pointer.1 - row.origin.y) / row.size.height.max(1.0);
    let title_at = row.size.height / 2.0 / height.max(1.0);
    let x = pointer.0 - width * held_x;
    let y = pointer.1 - height * (held_y + (title_at - held_y) * grown);
    rect(x, y, width, height)
}

/// What a thread's own window is opened with.
pub(crate) struct OwnWindowRequest {
    pub(crate) thread_id: WorkspaceThreadId,
    pub(crate) frame_slot: u32,
    /// Where it was asked to open: the top left of its content. By default
    /// it opens where the frame slot last was.
    pub(crate) position: Option<ScreenPoint>,
}

/// What a thread's own window does with a key assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnWindowAssignment {
    /// It concerns this window's terminals.
    Perform,
    /// It is about the threads and Spaces around this one, which this window
    /// does not show: a window that does gets it.
    HandOn,
    /// It has no meaning in a window with no sidebars.
    Ignore,
}

pub(crate) fn own_window_assignment(assignment: &KeyAssignment) -> OwnWindowAssignment {
    use KeyAssignment::*;
    match assignment {
        ActivateWorkspaceThread { .. }
        | ActivateWorkspaceThreadByIndex(_)
        | ActivateSpaceRelative(_)
        | SwitchSpace(_)
        | CreateSpace
        | CreateSpaceOnDomain(_)
        | CreateWorkspaceThread(_)
        | CreateWorkspaceThreadInActiveProject
        | SwitchToWorkspace { .. }
        | SwitchWorkspaceRelative(_)
        | ToggleLiveOverview
        | ToggleLibrary
        | OpenThreadSearch
        | SpawnWindow
        | SpawnCommandInNewWindow(_) => OwnWindowAssignment::HandOn,
        ToggleWorkspaceSidebar | ToggleRightSidebar | FocusPluginPanel(_) => {
            OwnWindowAssignment::Ignore
        }
        _ => OwnWindowAssignment::Perform,
    }
}

/// Whether `assignment` needs a terminal in this window, or would put one in
/// the page's mux window, where no thread would ever find it.
pub(crate) fn needs_a_terminal(assignment: &KeyAssignment) -> bool {
    use KeyAssignment::*;
    matches!(
        assignment,
        SpawnTab(_)
            | SpawnTabToRight(_)
            | SpawnCommandInNewTab(_)
            | SplitHorizontal(_)
            | SplitVertical(_)
            | SplitPane(_)
            | ShowLauncher
            | ShowLauncherArgs(_)
            | ShowTabNavigator
            | Search(_)
            | ActivateCopyMode
            | QuickSelect
            | QuickSelectArgs(_)
            | CharSelect(_)
            | PaneSelect(_)
    )
}

impl TermWindow {
    /// Open `thread_id` in a window of its own, at `position` when that was
    /// chosen, or bring its window forward if it has one.
    pub(crate) fn open_thread_in_own_window(
        &mut self,
        thread_id: WorkspaceThreadId,
        position: Option<ScreenPoint>,
        window: &Window,
    ) {
        self.open_thread_in_own_window_then(thread_id, position, None, window);
    }

    /// As `open_thread_in_own_window`. A window showing the thread goes on
    /// to `return_to`, when it is running, rather than to the page.
    fn open_thread_in_own_window_then(
        &mut self,
        thread_id: WorkspaceThreadId,
        position: Option<ScreenPoint>,
        return_to: Option<WorkspaceThreadId>,
        window: &Window,
    ) {
        if self.dedicated_thread.is_some() {
            return;
        }
        if front_end().thread_is_in_own_window(&thread_id) {
            self.focus_thread_window(&thread_id);
            return;
        }
        let live_workspaces = Mux::get().iter_workspaces();
        let plan = workspace_threads::activation_plan_for_thread(&thread_id, &live_workspaces)
            .filter(|plan| !plan.needs_materialize);
        let Some(plan) = plan else {
            // Not running. Opening it here knows everything that can take: a
            // server to connect to, a folder to ask for. It moves out once
            // it is on screen.
            let return_to = if self.elsewhere_page.is_some() {
                None
            } else {
                // Recorded as soon as it was picked, it may be this very
                // thread, on its way here.
                workspace_threads::recorded_active_thread_for_space(&self.active_space_id)
                    .filter(|active| *active != thread_id)
            };
            self.thread_bound_for_own_window = Some(ThreadBoundForOwnWindow {
                thread_id: thread_id.clone(),
                position,
                return_to,
            });
            self.activate_workspace_thread(thread_id, window);
            return;
        };
        let Some(mux_window_id) =
            workspace_threads::window_to_show_in_workspace(&plan.workspace_name)
        else {
            return;
        };
        // Another window shows it, through a reference: that one has to let
        // go of it.
        if mux_window_id != self.mux_window_id {
            if let Some(showing) = front_end().gui_window_for_mux_window(mux_window_id) {
                showing
                    .window
                    .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                        if let Some(window) = term_window.window.clone() {
                            term_window.open_thread_in_own_window(thread_id, position, &window);
                        }
                    })));
                return;
            }
        }
        let Some(space_id) = workspace_threads::thread_space_id(&thread_id) else {
            return;
        };
        if !front_end().begin_thread_window(&thread_id) {
            return;
        }
        if mux_window_id == self.mux_window_id {
            let live_workspaces = Mux::get().iter_workspaces();
            let return_to = return_to.filter(|thread_id| {
                workspace_threads::activation_plan_for_thread(thread_id, &live_workspaces)
                    .is_some_and(|plan| !plan.needs_materialize)
            });
            if let Some(return_to) = return_to {
                self.activate_workspace_thread(return_to, window);
            }
            // Another window may have that thread by now, and two cannot
            // share this one.
            if self.mux_window_id == mux_window_id {
                self.show_thread_elsewhere_page(thread_id.clone());
            }
        }
        // The reconcile must leave this one to us: it has a window coming.
        front_end().claim_spawned_mux_window(mux_window_id);
        let request = OwnWindowRequest {
            frame_slot: workspace_threads::note_thread_in_own_window(&thread_id),
            thread_id: thread_id.clone(),
            position,
        };
        let home = window.clone();
        promise::spawn::spawn(async move {
            match TermWindow::new_thread_window(mux_window_id, space_id, request).await {
                Ok(()) => {
                    if let Some(window) = front_end().thread_window(&thread_id) {
                        window.focus();
                    }
                }
                Err(err) => {
                    log::error!("failed to open a window for thread {thread_id}: {err:#}");
                    workspace_threads::note_thread_left_own_window(&thread_id);
                    front_end().thread_window_not_opened(&thread_id);
                }
            }
            home.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                term_window.thread_card_landing = None;
            })));
        })
        .detach();
    }

    /// Open again the thread windows the app was left with, one at a time:
    /// a thread that is not running goes through this window first.
    pub(crate) fn begin_reopening_thread_windows(&mut self) {
        self.threads_to_reopen =
            VecDeque::from(workspace_threads::threads_to_reopen_in_own_windows());
        self.reopen_next_thread_window();
    }

    fn reopen_next_thread_window(&mut self) {
        if self.local_thread_activation.is_some() || self.thread_bound_for_own_window.is_some() {
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        while let Some(thread_id) = self.threads_to_reopen.pop_front() {
            if !self.can_reopen_thread_window(&thread_id) {
                continue;
            }
            self.open_thread_in_own_window(thread_id, None, &window);
            window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                term_window.reopen_next_thread_window();
            })));
            return;
        }
    }

    /// A thread that is running can go straight to its window. One that is
    /// not has to open here, which only a thread of this Space can, and no
    /// server is woken for a window the person did not just ask for.
    fn can_reopen_thread_window(&self, thread_id: &str) -> bool {
        let live_workspaces = Mux::get().iter_workspaces();
        let running = workspace_threads::activation_plan_for_thread(thread_id, &live_workspaces)
            .is_some_and(|plan| !plan.needs_materialize);
        if running {
            return true;
        }
        let here = workspace_threads::thread_space_id(thread_id).as_deref()
            == Some(self.active_space_id.as_str());
        let local = workspace_threads::thread_connection_state(thread_id, &live_workspaces)
            .is_some_and(|state| !state.is_remote)
            && workspace_threads::client_domain_for_space(&self.active_space_id).is_none();
        here && local
    }

    /// A dragged thread row has left the sidebar, or come back.
    pub(crate) fn aim_thread_tear_off(&mut self, thread_id: &str, out: bool) {
        let Some(state) = self.sidebar_row_drag.as_mut() else {
            return;
        };
        if !state.active {
            return;
        }
        let now = Instant::now();
        match state.tear_off.as_mut() {
            Some(tear_off) => tear_off.aim(out, now),
            None if out => {
                state.tear_off = Some(ThreadTearOff::new(thread_preview(thread_id), now));
            }
            None => {}
        }
    }

    /// Fit the card's window to how far out it is and where the pointer is.
    pub(crate) fn move_thread_card_window(&self) {
        let Some(state) = self.sidebar_row_drag.as_ref() else {
            return;
        };
        let Some(tear_off) = state.tear_off.as_ref() else {
            return;
        };
        if let TearOffPlace::Out { card, .. } = &tear_off.place {
            let rect =
                self.thread_tear_off_card(&state.grip, state.current, tear_off.grown(Instant::now()));
            self.place_thread_card_window(&card.window, &card.size, rect, state.current);
        }
    }

    fn place_thread_card_window(
        &self,
        window: &Window,
        size: &Cell<(usize, usize)>,
        rect: RectF,
        pointer: ::window::Point,
    ) {
        let wanted = (
            rect.size.width.round() as usize,
            rect.size.height.round() as usize,
        );
        if size.replace(wanted) != wanted {
            window.resize_floating(wanted.0 as f32, wanted.1 as f32);
        }
        if let Some((event, home)) = self.current_mouse_event.as_ref().zip(self.window.as_ref()) {
            let margin = self.ui_f32(TEAR_OFF_CARD_MARGIN);
            let to_screen = home.screen_units_per_pixel();
            window.set_window_position(ScreenPoint::new(
                event.screen_coords.x
                    - ((pointer.x as f32 - rect.origin.x + margin) * to_screen).round() as isize,
                event.screen_coords.y
                    - ((pointer.y as f32 - rect.origin.y + margin) * to_screen).round() as isize,
            ));
        }
    }

    /// Windows and Linux stop at this window's edge until this has been
    /// tried on them. Wayland lets no program place a window, and the whole
    /// screen is this window when it is full screen.
    fn thread_card_can_leave_window(&self) -> bool {
        cfg!(target_os = "macos")
            && self.webgpu.is_some()
            && !self.window_state.contains(WindowState::FULL_SCREEN)
    }

    /// Render the whole card for a window of its own, and have that made.
    fn send_thread_card_out(&mut self) -> anyhow::Result<TearOffPlace> {
        let state = self
            .sidebar_row_drag
            .as_ref()
            .context("a tear-off card with no drag")?;
        let title = state.title.clone();
        let row_height = state.grip.height as usize;
        let crate::termwindow::SidebarRowKind::Thread { thread_id, .. } = &state.kind else {
            anyhow::bail!("a tear-off card with no thread");
        };
        let thread_id = thread_id.clone();
        let glass = crate::settings_window::liquid_glass_available();
        let card = self.thread_tear_off_card(&state.grip, state.current, 1.0);
        let started = state
            .tear_off
            .as_ref()
            .context("a tear-off card with no tear-off")?
            .started;

        let mut heap = HeapQuadAllocator::default();
        self.paint_tear_off_chrome(
            &mut TripleLayerQuadAllocator::Heap(&mut heap),
            card,
            &thread_id,
            &title,
            row_height,
            0.0,
            !glass,
            1.0,
        )?;
        let webgpu = self.webgpu.as_ref().context("no webgpu state")?;
        let texture = Rc::new(CardRenderTexture::copyable(
            (card.size.width.ceil() as u32).max(1),
            (card.size.height.ceil() as u32).max(1),
            webgpu,
        )?);
        let (first_vertex, quad_count) = {
            let mut frame_verts = self.card_frame_verts.borrow_mut();
            let base = frame_verts.len();
            heap.extract_vertices(&mut frame_verts);
            (base, (frame_verts.len() - base) / 4)
        };
        self.pending_card_renders
            .borrow_mut()
            .push(PendingCardRender {
                texture: Rc::clone(&texture),
                first_vertex,
                quad_count,
                area: card,
                corner_radius: 0.0,
            });

        let dimensions = Dimensions {
            pixel_width: texture.width as usize,
            pixel_height: texture.height as usize,
            dpi: self.dimensions.dpi,
        };
        let window = self
            .window
            .clone()
            .context("a tear-off card with no window")?;
        let config = self.config.adjusted(|config| {
            config.window_background_opacity = 0.0;
            config.macos_window_background_blur = 0;
            // The shadow is the card's own: the system's comes with a line
            // round the window.
            config.window_decorations =
                WindowDecorations::NONE | WindowDecorations::MACOS_FORCE_DISABLE_SHADOW;
            config.macos_window_glass = if glass {
                MacosWindowGlass::Clear
            } else {
                MacosWindowGlass::Blur
            };
        });
        let fonts = Rc::clone(&self.fonts);
        let geometry = RequestedWindowGeometry {
            width: Dimension::Pixels(dimensions.pixel_width as f32),
            height: Dimension::Pixels(dimensions.pixel_height as f32),
            ..Default::default()
        };
        promise::spawn::spawn(async move {
            let opened = Window::new_window(
                &get_window_class(),
                "",
                geometry,
                Some(&config),
                fonts,
                |_, _| {},
            )
            .await;
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                match opened {
                    Ok(card) => term_window.thread_card_window_opened(started, card),
                    Err(err) => {
                        log::warn!("no window for the tear-off card: {err:#}");
                        term_window.keep_thread_card_here(started);
                    }
                }
            })));
        })
        .detach();

        Ok(TearOffPlace::Leaving {
            chrome: texture,
            dimensions,
        })
    }

    fn keep_thread_card_here(&mut self, started: Instant) {
        if let Some(tear_off) = self.thread_tear_off_mut(started) {
            tear_off.place = TearOffPlace::Here;
        }
        self.invalidate_window();
    }

    fn thread_tear_off_mut(&mut self, started: Instant) -> Option<&mut ThreadTearOff> {
        self.sidebar_row_drag
            .as_mut()
            .and_then(|state| state.tear_off.as_mut())
            .filter(|tear_off| tear_off.started == started)
    }

    fn thread_card_window_opened(&mut self, started: Instant, window: Window) {
        let leaving = self
            .thread_tear_off_mut(started)
            .map(|tear_off| std::mem::replace(&mut tear_off.place, TearOffPlace::Here));
        let Some(TearOffPlace::Leaving { chrome, dimensions }) = leaving else {
            // The drag ended while it was being made.
            window.close();
            return;
        };
        match self.show_thread_card_window(&window, chrome, dimensions) {
            Ok(card) => {
                if let Some(tear_off) = self.thread_tear_off_mut(started) {
                    tear_off.place = TearOffPlace::Out {
                        card,
                        since: Instant::now(),
                    };
                }
            }
            Err(err) => {
                log::warn!("the tear-off card stays in its window: {err:#}");
                window.close();
            }
        }
        self.invalidate_window();
    }

    fn show_thread_card_window(
        &self,
        window: &Window,
        chrome: Rc<CardRenderTexture>,
        dimensions: Dimensions,
    ) -> anyhow::Result<ThreadCardWindow> {
        let webgpu = self.webgpu.as_ref().context("no webgpu state")?;
        let (surface, handle) = webgpu.surface_for(
            window,
            dimensions.pixel_width as u32,
            dimensions.pixel_height as u32,
        )?;
        window.set_window_level(WindowLevel::AlwaysOnTop);
        let state = self
            .sidebar_row_drag
            .as_ref()
            .context("a tear-off card with no drag")?;
        let grown = state
            .tear_off
            .as_ref()
            .map_or(0.0, |tear_off| tear_off.grown(Instant::now()));
        let rect = self.thread_tear_off_card(&state.grip, state.current, grown);
        let width = rect.size.width.round();
        let height = rect.size.height.round();
        let scale = self
            .window
            .as_ref()
            .map_or(1.0, |home| home.pixels_per_point());
        window.show_floating(FloatingCard {
            fade_in: TEAR_OFF_HANDOVER,
            scale,
            width,
            height,
            full_width: dimensions.pixel_width as f32,
            full_height: dimensions.pixel_height as f32,
            margin: self.ui_f32(TEAR_OFF_CARD_MARGIN),
            corner_radius: self.ui_f32(TEAR_OFF_CARD_RADIUS),
            title_height: state.grip.height as f32,
            thumbnail_inset: self.ui_f32(TEAR_OFF_CARD_PADDING),
            thumbnail_radius: self.ui_f32(TEAR_OFF_THUMBNAIL_RADIUS),
        });
        let size = Cell::new((width as usize, height as usize));
        self.place_thread_card_window(window, &size, rect, state.current);
        show_card_on_surface(webgpu, &surface, dimensions, &chrome, None)?;
        Ok(ThreadCardWindow {
            window: window.clone(),
            surface,
            _handle: handle,
            chrome,
            dimensions,
            size,
            has_thumbnail: false,
        })
    }

    /// The picture is ready, in a frame already submitted: put it on the card.
    fn show_thumbnail_on_thread_card(&mut self) {
        let Some(webgpu) = self.webgpu.clone() else {
            return;
        };
        let padding = self.ui_f32(TEAR_OFF_CARD_PADDING);
        let picture = self.sidebar_row_drag.as_ref().and_then(|state| {
            let (tab_id, _) = state.tear_off.as_ref()?.preview.as_ref()?;
            Some((self.preview_picture(*tab_id)?, state.grip.height as f32))
        });
        let Some((texture, title_height)) = picture else {
            return;
        };
        let Some(state) = self.sidebar_row_drag.as_mut() else {
            return;
        };
        let Some(TearOffPlace::Out { card, .. }) =
            state.tear_off.as_mut().map(|tear_off| &mut tear_off.place)
        else {
            return;
        };
        let dest = rect(
            padding,
            title_height,
            texture.width as f32,
            texture.height as f32,
        );
        let thumbnail = CardComposite {
            texture,
            dest,
            clip: dest,
            opacity: 1.0,
            zindex: 0,
        };
        let shown = show_card_on_surface(
            &webgpu,
            &card.surface,
            card.dimensions,
            &card.chrome,
            Some(&thumbnail),
        );
        match shown {
            Ok(()) => card.has_thumbnail = true,
            Err(err) => log::warn!("no thumbnail on the tear-off card: {err:#}"),
        }
    }

    pub(crate) fn drop_thread_out_of_sidebar(
        &mut self,
        mut state: crate::termwindow::SidebarRowDragState,
        pointer_on_screen: ScreenPoint,
    ) {
        self.sidebar_make_way.finish(false);
        // Stays up until there is a window to take its place.
        let card_window = state
            .tear_off
            .take()
            .and_then(ThreadTearOff::into_card_window);
        let crate::termwindow::SidebarRowKind::Thread { thread_id, .. } = state.kind else {
            return;
        };
        let Some(window) = self.window.clone() else {
            return;
        };
        let card = self.thread_tear_off_card(&state.grip, state.current, 1.0);
        let to_screen = window.screen_units_per_pixel();
        let position = ScreenPoint::new(
            pointer_on_screen.x
                - ((state.current.x as f32 - card.origin.x) * to_screen) as isize,
            pointer_on_screen.y
                - ((state.current.y as f32 - card.origin.y) * to_screen) as isize,
        );
        let had_window = front_end().thread_is_in_own_window(&thread_id);
        self.open_thread_in_own_window(thread_id.clone(), Some(position), &window);
        // Only a window on its way takes over from the card, and says when.
        if !had_window && front_end().thread_is_in_own_window(&thread_id) {
            self.thread_card_landing = card_window;
        }
    }

    fn thread_tear_off_card(
        &self,
        grip: &crate::termwindow::DragGrip,
        pointer: ::window::Point,
        grown: f32,
    ) -> RectF {
        let row = rect(
            pointer.x as f32 - grip.x,
            pointer.y as f32 - grip.y,
            grip.width as f32,
            grip.height as f32,
        );
        let width = self.ui_f32(TEAR_OFF_CARD_WIDTH);
        let terminal = self.terminal_content_rect();
        let aspect = terminal.size.height / terminal.size.width.max(1.0);
        tear_off_card_rect(
            row,
            (pointer.x as f32, pointer.y as f32),
            (width, width * aspect),
            grown,
        )
    }

    /// The card under the pointer while a thread row is out of the sidebar.
    /// False when no row is.
    fn tear_off_surface(&self) -> window::color::LinearRgba {
        let chrome = self.chrome();
        crate::termwindow::render::paint::drag_ghost_surface(
            chrome.sidebar_row_active_bg,
            self.sidebar_surface(chrome.workspace_sidebar_bg),
        )
    }

    /// The card's surface and the row it came from, for this window or for
    /// its own.
    fn paint_tear_off_chrome(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        card: RectF,
        thread_id: &str,
        title: &str,
        row_height: usize,
        grown: f32,
        solid: bool,
        opacity: f32,
    ) -> anyhow::Result<()> {
        let chrome = self.chrome();
        let surface = self.tear_off_surface();
        if solid {
            let radius = self.ui_f32(TEAR_OFF_CARD_RADIUS);
            let gl_state = self
                .render_state
                .as_ref()
                .context("tear-off card without a render state")?;
            let ctx = DrawContext::new(gl_state, self.dimensions, &self.render_metrics);
            let shadow = window::color::LinearRgba::with_components(
                0.0,
                0.0,
                0.0,
                opacity * grown * if chrome.is_dark() { 0.5 } else { 0.22 },
            );
            ctx.draw_elevated_surface(
                layers,
                0,
                card,
                surface.mul_alpha(opacity),
                chrome.sidebar_row_active_border.mul_alpha(opacity),
                shadow,
                radius,
            )?;
        }
        let mut row = HeapQuadAllocator::default();
        self.paint_thread_row_title(
            &mut TripleLayerQuadAllocator::Heap(&mut row),
            thread_id,
            title,
            card.size.width as usize,
            row_height,
            surface,
        )?;
        let clip = QuadClipRect::from_top_left_pixels(
            card.min_x(),
            card.min_y(),
            card.max_x(),
            card.max_y(),
            &self.dimensions,
        );
        row.apply_to_clipped_at(layers, card.origin.x, card.origin.y, clip, opacity)
    }

    pub(crate) fn paint_thread_tear_off(&mut self, now: Instant) -> anyhow::Result<bool> {
        let Some(state) = self
            .sidebar_row_drag
            .as_ref()
            .filter(|state| state.active && state.tear_off.is_some())
        else {
            return Ok(false);
        };
        let Some(tear_off) = state.tear_off.as_ref() else {
            return Ok(false);
        };
        // Back at its row, it is the row again.
        if !tear_off.out && tear_off.settled(now) {
            if let Some(state) = self.sidebar_row_drag.as_mut() {
                state.tear_off = None;
            }
            return Ok(false);
        }
        let grown = tear_off.grown(now);
        let travelling = !tear_off.settled(now);
        // Going, under the window that is taking over from it.
        let mut opacity = 1.0;
        let mut needs_thumbnail = true;
        let mut in_own_window = false;
        if let TearOffPlace::Out { card, since } = &tear_off.place {
            let gone = now.saturating_duration_since(*since).as_secs_f32()
                / TEAR_OFF_HANDOVER.as_secs_f32();
            opacity = (1.0 - gone).max(0.0);
            needs_thumbnail = !card.has_thumbnail;
            in_own_window = true;
        }
        let growing = matches!(tear_off.place, TearOffPlace::Growing);
        let preview = tear_off.preview.clone();
        let title = state.title.clone();
        let row_height = state.grip.height as usize;
        let thread_id = match &state.kind {
            crate::termwindow::SidebarRowKind::Thread { thread_id, .. } => thread_id.clone(),
            _ => return Ok(false),
        };
        let full_size = self
            .thread_tear_off_card(&state.grip, state.current, 1.0)
            .size;
        let mut card = self.thread_tear_off_card(&state.grip, state.current, grown);
        // Whole inside the window, while this window is the one drawing it.
        let room = (
            (self.dimensions.pixel_width as f32 - card.size.width).max(0.0),
            (self.dimensions.pixel_height as f32 - card.size.height).max(0.0),
        );
        card.origin.x = card.origin.x.clamp(0.0, room.0);
        card.origin.y = card.origin.y.clamp(0.0, room.1);
        if travelling {
            self.move_thread_card_window();
        }
        if travelling || (in_own_window && opacity > 0.0) {
            self.update_next_frame_time(Some(now + Duration::from_millis(16)));
        }
        if opacity <= 0.0 && !needs_thumbnail {
            return Ok(true);
        }

        let padding = self.ui_f32(TEAR_OFF_CARD_PADDING);
        let layer = {
            let gl_state = self
                .render_state
                .as_ref()
                .context("tear-off card without a render state")?;
            gl_state.layer_for_zindex(crate::termwindow::DRAG_GHOST_ZINDEX)?
        };
        let mut layers = layer.quad_allocator();
        if opacity > 0.0 {
            self.paint_tear_off_chrome(
                &mut layers,
                card,
                &thread_id,
                &title,
                row_height,
                grown,
                true,
                opacity,
            )?;
        }
        // On glass the card's window cuts the corners, as the system does.
        let corner_radius = if crate::settings_window::liquid_glass_available() {
            0.0
        } else {
            self.ui_f32(TEAR_OFF_THUMBNAIL_RADIUS)
        };
        let background = Some(self.palette().background.to_linear());
        let mut thumbnail_ready = false;
        if let Some((tab_id, snapshot)) = preview {
            let title_height = row_height as f32;
            let width = full_size.width - padding * 2.0;
            let height = (full_size.height - title_height - padding).max(0.0);
            if height > 0.0 {
                // Under way from the moment the row comes away, so as to be
                // there when the card has grown to take it. A big terminal
                // takes more than a frame, and is put together in place: it
                // has to be built where it stays put, not on a moving card.
                let area = rect(0.0, 0.0, width, height);
                let request = TerminalPreviewRequest {
                    tab_id,
                    snapshot,
                    area,
                    clip: area,
                    hold_scale: false,
                    opacity: 1.0,
                    prebuild: true,
                    corner_radius,
                    background,
                };
                self.paint_terminal_previews(&mut layers, std::slice::from_ref(&request))?;
                let recorded = self.preview_thumbnail_recorded(tab_id);
                let picture = self.preview_picture(tab_id);
                thumbnail_ready = picture.is_some();
                if !recorded {
                    self.update_next_frame_time(Some(now + Duration::from_millis(16)));
                } else if grown >= 1.0 && opacity > 0.0 {
                    let dest = rect(
                        card.origin.x + padding,
                        card.origin.y + title_height,
                        width,
                        height,
                    );
                    match picture {
                        // Over this card, not under it with the terminal behind.
                        Some(texture) => self.card_composites.borrow_mut().push(CardComposite {
                            texture,
                            dest,
                            clip: dest,
                            opacity,
                            zindex: crate::termwindow::DRAG_GHOST_ZINDEX,
                        }),
                        // OpenGL keeps it as quads, to play back where the
                        // card is now. Nothing cuts their corners, so the
                        // card paints back over them.
                        None => {
                            self.paint_terminal_previews(
                                &mut layers,
                                &[TerminalPreviewRequest {
                                    area: dest,
                                    clip: dest,
                                    prebuild: false,
                                    opacity,
                                    ..request
                                }],
                            )?;
                            let surface = self.tear_off_surface().mul_alpha(opacity);
                            let radius = self.ui_f32(TEAR_OFF_THUMBNAIL_RADIUS);
                            let gl_state = self
                                .render_state
                                .as_ref()
                                .context("tear-off card without a render state")?;
                            DrawContext::new(gl_state, self.dimensions, &self.render_metrics)
                                .draw_rounded_preview_chrome(
                                    &mut layers,
                                    2,
                                    dest,
                                    dest,
                                    surface,
                                    None,
                                    radius,
                                )?;
                        }
                    }
                }
            }
        }
        drop(layers);
        if in_own_window && needs_thumbnail && thumbnail_ready {
            if let Some(window) = self.window.as_ref() {
                window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                    term_window.show_thumbnail_on_thread_card();
                })));
            }
        }
        if growing {
            let place = if self.thread_card_can_leave_window() {
                self.send_thread_card_out().unwrap_or_else(|err| {
                    log::warn!("the tear-off card stays in its window: {err:#}");
                    TearOffPlace::Here
                })
            } else {
                TearOffPlace::Here
            };
            if let Some(tear_off) = self
                .sidebar_row_drag
                .as_mut()
                .and_then(|state| state.tear_off.as_mut())
            {
                tear_off.place = place;
            }
        }
        Ok(true)
    }

    pub(crate) fn focus_thread_window(&self, thread_id: &str) {
        if let Some(window) = front_end().thread_window(thread_id) {
            window.focus();
        }
    }

    /// What a page's button does: to the thread's own window or, with that
    /// gone, to the window that took the thread back. Activating comes to
    /// that window, or opens the thread here if none did.
    fn go_to_thread_on_page(&mut self, thread_id: WorkspaceThreadId) {
        if front_end().thread_is_in_own_window(&thread_id) {
            self.focus_thread_window(&thread_id);
        } else if let Some(window) = self.window.clone() {
            self.activate_workspace_thread(thread_id, &window);
        }
    }

    /// Hand `assignment` to a window that shows the thread list, from one
    /// that does not.
    pub(crate) fn hand_on_assignment(&self, assignment: KeyAssignment) {
        let Some(target) = front_end().window_for_any_thread(&self.active_space_id) else {
            // Only threads' own windows are left: the next try has a window
            // to go to.
            front_end().spawn_space_window();
            return;
        };
        target.window.focus();
        target
            .window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                if let Some(pane) = term_window.pane_for_key_bindings() {
                    if let Err(err) = term_window.perform_key_assignment(&pane, &assignment) {
                        log::error!("{assignment:?} failed: {err:#}");
                    }
                }
            })));
    }

    /// Hand `assignment` to the window of the thread this window's page
    /// stands in for: it asks something of a terminal, which is there.
    pub(crate) fn hand_to_thread_on_page(&self, assignment: KeyAssignment) {
        let Some(window) = self
            .elsewhere_page
            .as_ref()
            .and_then(|page| front_end().thread_window(&page.thread_id))
        else {
            return;
        };
        window.focus();
        window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
            if let Some(pane) = term_window.pane_for_key_bindings() {
                if let Err(err) = term_window.perform_key_assignment(&pane, &assignment) {
                    log::error!("{assignment:?} failed: {err:#}");
                }
            }
        })));
    }

    /// The pane key bindings run against: the terminal's, or the page's
    /// stand-in for it.
    pub(crate) fn pane_for_key_bindings(&self) -> Option<Arc<dyn Pane>> {
        self.get_active_pane_or_overlay().or_else(|| {
            self.elsewhere_page
                .as_ref()
                .and_then(|page| page.key_pane.clone())
        })
    }

    /// The window has a thread to show that may be open in its own window.
    /// A thread picked in the sidebar, by its row or its number. One open in
    /// a window of its own shows its page here, as when this window lands on
    /// it by itself: going over there is the page's button.
    pub(crate) fn pick_sidebar_thread(
        &mut self,
        thread_id: WorkspaceThreadId,
        context: &dyn WindowOps,
    ) {
        if self.dedicated_thread.is_none() && front_end().thread_is_in_own_window(&thread_id) {
            self.forget_thread_bound_for_own_window_unless(&thread_id);
            self.local_thread_activation = None;
            self.workspace_sidebar_pending_thread_selection = None;
            self.show_thread_or_its_page(thread_id, context);
            context.invalidate();
        } else if !self.open_remote_workspace_thread_without_connecting(&thread_id, context) {
            self.activate_workspace_thread(thread_id, context);
        }
    }

    pub(crate) fn show_thread_or_its_page(
        &mut self,
        thread_id: WorkspaceThreadId,
        context: &dyn WindowOps,
    ) {
        if front_end().thread_is_in_own_window(&thread_id) {
            // In this order: leaving a thread saves its layout under the
            // pointers the record is about to move.
            self.show_thread_elsewhere_page(thread_id.clone());
            self.note_thread_on_page(&thread_id);
        } else {
            self.activate_workspace_thread(thread_id, context);
        }
    }

    /// What this window has selected, for its sidebar and the next start,
    /// though the thread itself stays where it is.
    fn note_thread_on_page(&self, thread_id: &str) {
        let own_space = workspace_threads::thread_space_id(thread_id).as_deref()
            == Some(self.active_space_id.as_str());
        if own_space {
            workspace_threads::clear_thread_active_ref(&self.active_space_id);
        } else {
            workspace_threads::note_thread_active_ref(&self.active_space_id, thread_id);
        }
        workspace_threads::activate_thread_record(
            thread_id,
            &Mux::get().iter_workspaces(),
            own_space,
        );
    }

    /// Show the page for `thread_id` in place of whatever this window shows.
    pub(crate) fn show_thread_elsewhere_page(&mut self, thread_id: WorkspaceThreadId) {
        let title = self.thread_page_title(&thread_id);
        let view = Box::new(ThreadElsewhereView::new(thread_id.clone(), title));
        if let Some(page) = self.elsewhere_page.as_mut() {
            // Already on a page: the same one, or another thread's.
            if page.thread_id == thread_id {
                let view_id = page.view_id;
                self.set_active_content_view_id(Some(view_id));
                return;
            }
            let old_view_id = page.view_id;
            page.thread_id = thread_id;
            self.close_content_view_by_id(old_view_id);
            let view_id = self.open_content_view(view);
            if let Some(page) = self.elsewhere_page.as_mut() {
                page.view_id = view_id;
            }
            return;
        }

        self.snapshot_active_workspace_thread_layout();
        let mux = Mux::get();
        let workspace = format!(
            "{}{}",
            workspace_threads::THREAD_PAGE_WORKSPACE_PREFIX,
            self.space_owner_id
        );
        let builder = mux.new_empty_window_for_domain(Some(workspace.clone()), None, None);
        let page_window_id = *builder;
        mux.register_window_ui_surface(page_window_id, THREAD_PAGE_SURFACE.to_string());
        // Shown here before the frontend hears of it, so the reconcile finds
        // it taken instead of opening a window of its own for it. The active
        // workspace stays out of it: that is where new terminals go, and
        // this one is to have none.
        self.switch_to_mux_window(page_window_id);
        drop(builder);
        let key_pane = self.inert_key_pane();
        let view_id = self.open_content_view(view);
        self.elsewhere_page = Some(ThreadElsewherePage {
            thread_id,
            mux_window_id: page_window_id,
            view_id,
            key_pane,
        });
    }

    fn inert_key_pane(&self) -> Option<Arc<dyn Pane>> {
        let term_config = Arc::new(TermConfig::with_config(self.config.clone()));
        match mux::termwiztermtab::inert_pane(self.terminal_size, term_config) {
            Ok(pane) => Some(pane),
            Err(err) => {
                log::error!("no pane for key bindings on the thread page: {err:#}");
                None
            }
        }
    }

    fn thread_page_title(&self, thread_id: &str) -> String {
        workspace_threads::thread_name(thread_id).unwrap_or_default()
    }

    /// This window is going to `next_mux_window_id`: a page it was on goes.
    pub(crate) fn leave_thread_elsewhere_page(&mut self, next_mux_window_id: MuxWindowId) {
        let leaving = self
            .elsewhere_page
            .as_ref()
            .is_some_and(|page| page.mux_window_id != next_mux_window_id);
        if !leaving {
            return;
        }
        let Some(page) = self.elsewhere_page.take() else {
            return;
        };
        self.close_content_view_by_id(page.view_id);
        // Empty now, so this is the end of the page's mux window.
        Mux::get().unregister_window_ui_surface(page.mux_window_id, THREAD_PAGE_SURFACE);
    }

    /// A thread's own window has gone. Every window hears of it: its sidebar
    /// row changes, and a page standing in for the thread has nothing to
    /// stand in for.
    pub(crate) fn thread_window_closed(&mut self, thread_id: &str) {
        self.invalidate_window();
        // A thread on its way here means the person has moved on already.
        let on_its_page = self.local_thread_activation.is_none()
            && self
                .elsewhere_page
                .as_ref()
                .is_some_and(|page| page.thread_id == thread_id);
        if !on_its_page {
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        let live_workspaces = Mux::get().iter_workspaces();
        let still_running =
            workspace_threads::activation_plan_for_thread(thread_id, &live_workspaces)
                .is_some_and(|plan| !plan.needs_materialize);
        if still_running {
            // Another page got there first. This one stays, and its button
            // goes to that window.
            let taken = workspace_threads::thread_workspace_name(thread_id)
                .is_some_and(|workspace| self.workspace_shown_elsewhere(&workspace));
            if !taken {
                self.activate_workspace_thread(thread_id.to_string(), &window);
            }
            return;
        }
        // It ended in its window: on to another, as when a window's own
        // thread ends, or out of a server's Space with none left to go to.
        let next = match workspace_threads::client_domain_for_space(&self.active_space_id) {
            Some(domain_name) => {
                let other = domain_is_attached(&domain_name)
                    .then(|| {
                        workspace_threads::existing_thread_to_recover_after_window_death(
                            &self.active_space_id,
                        )
                    })
                    .flatten();
                if other.is_none() {
                    let return_space_id = self
                        .previous_space_id
                        .clone()
                        .unwrap_or_else(workspace_threads::default_space_id);
                    self.return_from_unreachable_space(return_space_id, None, &window);
                    return;
                }
                other
            }
            None => workspace_threads::thread_to_recover_after_window_death(&self.active_space_id),
        };
        if let Some(next) = next {
            self.show_thread_or_its_page(next, &window);
        }
    }

    /// Whether `mux_window_id` being on screen here is what a thread bound
    /// for its own window was waiting for.
    pub(crate) fn send_on_thread_bound_for_own_window(&mut self, mux_window_id: MuxWindowId) {
        let arrived = self
            .thread_bound_for_own_window
            .as_ref()
            .is_some_and(|bound| {
                let Some(workspace) = workspace_threads::thread_workspace_name(&bound.thread_id)
                else {
                    return false;
                };
                Mux::get()
                    .get_window(mux_window_id)
                    .is_some_and(|window| window.get_workspace() == workspace)
            });
        let Some(window) = self.window.clone() else {
            return;
        };
        // The next thread only sets off after this one has gone: going back
        // to where this window was would call the next one off.
        if !arrived {
            if !self.threads_to_reopen.is_empty() {
                window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                    term_window.reopen_next_thread_window();
                })));
            }
            return;
        }
        let Some(bound) = self.thread_bound_for_own_window.take() else {
            return;
        };
        window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
            if let Some(window) = term_window.window.clone() {
                term_window.open_thread_in_own_window_then(
                    bound.thread_id,
                    bound.position,
                    bound.return_to,
                    &window,
                );
                term_window.reopen_next_thread_window();
            }
        })));
    }

    /// `thread_id` could not be got running: if it was bound for a window of
    /// its own it stays a row, and the rest go on without it.
    pub(crate) fn thread_bound_for_own_window_failed(&mut self, thread_id: &str) {
        if self
            .thread_bound_for_own_window
            .as_ref()
            .is_some_and(|bound| bound.thread_id == thread_id)
        {
            self.thread_bound_for_own_window = None;
            workspace_threads::note_thread_left_own_window(thread_id);
        }
        if !self.threads_to_reopen.is_empty() {
            if let Some(window) = self.window.as_ref() {
                window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                    term_window.reopen_next_thread_window();
                })));
            }
        }
    }

    /// Opening another thread here gives up on a thread bound for its own
    /// window.
    pub(crate) fn forget_thread_bound_for_own_window_unless(&mut self, thread_id: &str) {
        if self
            .thread_bound_for_own_window
            .as_ref()
            .is_some_and(|bound| bound.thread_id != thread_id)
        {
            self.thread_bound_for_own_window = None;
        }
    }
}

/// The running thread's terminal, to show on the card.
fn thread_preview(thread_id: &str) -> Option<(TabId, Arc<TerminalPreviewSnapshot>)> {
    let workspace = workspace_threads::thread_workspace_name(thread_id)?;
    let mux_window_id = workspace_threads::window_to_show_in_workspace(&workspace)?;
    let tab_id = Mux::get()
        .get_active_tab_for_window(mux_window_id)?
        .tab_id();
    let snapshot = crate::termwindow::live_overview::capture_terminal_snapshot(tab_id)?;
    Some((tab_id, Arc::new(snapshot)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThreadPageAction {
    GoToWindow,
}

/// What a window shows while a thread it would show is in its own window.
struct ThreadElsewhereView {
    thread_id: WorkspaceThreadId,
    title: String,
    widgets: UiContext<ThreadPageAction>,
    interaction: InteractionState<ThreadPageAction>,
}

impl ThreadElsewhereView {
    fn new(thread_id: WorkspaceThreadId, title: String) -> Self {
        Self {
            thread_id,
            title,
            widgets: UiContext::default(),
            interaction: InteractionState::default(),
        }
    }

    fn go_to_window(&self) -> ContentViewResponse {
        let thread_id = self.thread_id.clone();
        ContentViewResponse::Run(Box::new(move |term_window| {
            term_window.go_to_thread_on_page(thread_id);
        }))
    }

    fn button_state(&self) -> ControlState {
        let action = Some(ThreadPageAction::GoToWindow);
        if self.interaction.pressed == action {
            ControlState::Pressed
        } else if self.interaction.hovered == action {
            ControlState::Hovered
        } else {
            ControlState::Normal
        }
    }
}

impl ContentView for ThreadElsewhereView {
    fn title(&self) -> String {
        self.title.clone()
    }

    fn show_in_tab_bar(&self) -> bool {
        false
    }

    fn tab_key(&self) -> Option<String> {
        Some(format!("{THREAD_PAGE_VIEW_KEY_PREFIX}{}", self.thread_id))
    }

    fn paint(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        _title_font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        caption_font: &Rc<LoadedFont>,
        _cursor_on: bool,
    ) -> anyhow::Result<()> {
        self.widgets.clear();

        let center_x = area.origin.x + area.size.width / 2.0;
        let max_width = (area.size.width - ctx.px(PAD) * 2.0).max(0.0);
        let line_height = ctx.metrics.cell_size.height as f32;
        let icon_size = ctx.px(HERO_ICON_SIZE);
        let button_height = line_height + ctx.px(BUTTON_MARGIN) * 2.0;
        let title_gap = ctx.px(14.0);
        let note_gap = ctx.px(4.0);
        let button_gap = ctx.px(16.0);
        let stack_height = icon_size
            + title_gap
            + line_height
            + note_gap
            + line_height
            + button_gap
            + button_height;
        let mut y = area.origin.y + ((area.size.height - stack_height) / 2.0).max(0.0);

        // What the sidebar shows for the thread, as Settings has it laid out.
        let layout = crate::native_settings::sidebar_thread_icons();
        let by_workspace = crate::thread_marks::icons_by_workspace();
        let terminal = crate::tab_icons::resolve_window_tab();
        let icons = workspace_threads::thread_workspace_name(&self.thread_id)
            .and_then(|workspace| by_workspace.get(&workspace))
            .filter(|icons| !icons.is_empty())
            .map_or(std::slice::from_ref(&terminal), Vec::as_slice);
        let count_metrics =
            crate::utilsprites::RenderMetrics::with_font_metrics(&caption_font.metrics());
        let icon_width = crate::thread_marks::drawn_width(layout, icons.len(), icon_size);
        crate::thread_marks::draw_slot(
            ctx,
            layers,
            layout,
            icons,
            center_x - icon_width / 2.0,
            y + icon_size / 2.0,
            icon_size,
            &crate::thread_marks::IconStyle {
                surface: palette.window_bg,
                dark: palette.is_dark(),
                count_font: caption_font,
                count_metrics: &count_metrics,
                count_color: palette.muted_text,
                opacity: 1.0,
            },
        )?;
        y += icon_size + title_gap;

        let name = workspace_threads::thread_name(&self.thread_id);
        let name = name.as_deref().unwrap_or(&self.title);
        let title = ctx.text_with_ellipsis(section_font, name, max_width);
        let title_width = ctx.measure_text_width(section_font, &title);
        ctx.draw_text(
            layers,
            section_font,
            center_x - title_width / 2.0,
            y,
            &title,
            palette.text,
            max_width,
        )?;
        y += line_height + note_gap;

        let note = crate::i18n::tr("thread-window-page-note");
        let note = ctx.text_with_ellipsis(font, &note, max_width);
        let note_width = ctx.measure_text_width(font, &note);
        ctx.draw_text(
            layers,
            font,
            center_x - note_width / 2.0,
            y,
            &note,
            palette.muted_text,
            max_width,
        )?;
        y += line_height + button_gap;

        let label = crate::i18n::tr("thread-window-go-to");
        let label_width = ctx.measure_text_width(font, &label);
        let button_width = (label_width + ctx.px(BUTTON_PADDING) * 2.0).min(max_width);
        let button = rect(
            center_x - button_width / 2.0,
            y,
            button_width,
            button_height,
        );
        self.widgets
            .push(button, WidgetKind::Button, ThreadPageAction::GoToWindow);
        let fill = palette.accent.mul_alpha(match self.button_state() {
            ControlState::Pressed => 0.32,
            ControlState::Hovered => 0.24,
            _ => 0.16,
        });
        ctx.draw_rounded_frame(
            layers,
            0,
            button.origin.x,
            button.origin.y,
            button.size.width,
            button.size.height,
            fill,
            fill,
            button.size.height / 2.0,
        )?;
        ctx.draw_text(
            layers,
            font,
            center_x - label_width / 2.0,
            button.origin.y + ((button.size.height - line_height) / 2.0).max(0.0),
            &label,
            palette.accent,
            button.size.width,
        )?;
        Ok(())
    }

    fn on_mouse(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse {
        let hit = self.widgets.hit_test(x, y).map(|target| target.action);
        match kind {
            WMEK::Move => {
                if self.interaction.hovered != hit {
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
                    return self.go_to_window();
                }
                ContentViewResponse::Redraw
            }
            _ => ContentViewResponse::Ignored,
        }
    }

    fn on_key(&mut self, key: KeyCode, mods: KeyModifiers) -> ContentViewResponse {
        match key {
            KeyCode::Enter if mods == KeyModifiers::NONE => self.go_to_window(),
            _ => ContentViewResponse::Ignored,
        }
    }

    /// The page goes when the window moves on, not by being closed.
    fn on_close_requested(&mut self) -> ContentViewResponse {
        ContentViewResponse::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::keyassignment::SpawnTabDomain;

    #[test]
    fn a_threads_own_window_hands_on_what_is_about_other_threads() {
        for assignment in [
            KeyAssignment::ActivateWorkspaceThreadByIndex(2),
            KeyAssignment::ActivateSpaceRelative(1),
            KeyAssignment::CreateWorkspaceThreadInActiveProject,
            KeyAssignment::ToggleLiveOverview,
            KeyAssignment::SpawnWindow,
        ] {
            assert_eq!(
                own_window_assignment(&assignment),
                OwnWindowAssignment::HandOn,
                "{assignment:?}"
            );
        }
    }

    #[test]
    fn a_threads_own_window_has_no_sidebars_to_toggle() {
        for assignment in [
            KeyAssignment::ToggleWorkspaceSidebar,
            KeyAssignment::ToggleRightSidebar,
        ] {
            assert_eq!(
                own_window_assignment(&assignment),
                OwnWindowAssignment::Ignore,
                "{assignment:?}"
            );
        }
    }

    #[test]
    fn a_threads_own_window_keeps_what_is_about_its_terminals() {
        assert_eq!(
            own_window_assignment(&KeyAssignment::SpawnTab(SpawnTabDomain::CurrentPaneDomain)),
            OwnWindowAssignment::Perform
        );
    }

    #[test]
    fn a_card_leaving_a_row_starts_as_the_row_and_keeps_it_under_the_pointer() {
        let row = rect(10.0, 40.0, 200.0, 30.0);
        let pointer = (60.0, 55.0);
        let card = tear_off_card_rect(row, pointer, (300.0, 200.0), 0.0);
        assert_eq!((card.origin.x, card.origin.y), (10.0, 40.0));
        assert_eq!((card.size.width, card.size.height), (200.0, 30.0));
        for grown in [0.0, 0.25, 0.5, 1.0] {
            let card = tear_off_card_rect(row, pointer, (300.0, 200.0), grown);
            assert!(
                card.origin.x <= pointer.0 && pointer.0 <= card.max_x(),
                "at {}",
                grown
            );
            assert!(
                card.origin.y <= pointer.1 && pointer.1 <= card.max_y(),
                "at {}",
                grown
            );
        }
    }

    #[test]
    fn a_card_turning_back_carries_on_from_where_it_had_got_to() {
        let start = Instant::now();
        let mut tear_off = ThreadTearOff::new(None, start);
        assert_eq!(tear_off.grown(start), 0.0);
        assert_eq!(tear_off.grown(start + TEAR_OFF_GROW), 1.0);

        let halfway = start + TEAR_OFF_GROW / 2;
        let reached = tear_off.grown(halfway);
        tear_off.aim(false, halfway);
        assert_eq!(tear_off.grown(halfway), reached);
        assert!(!tear_off.settled(halfway));
        assert_eq!(tear_off.grown(halfway + TEAR_OFF_GROW), 0.0);
        assert!(tear_off.settled(halfway + TEAR_OFF_GROW));
    }

    #[test]
    fn a_grown_card_is_held_by_its_title() {
        let row = rect(10.0, 40.0, 200.0, 30.0);
        let card = tear_off_card_rect(row, (60.0, 55.0), (300.0, 200.0), 1.0);
        assert_eq!((card.size.width, card.size.height), (300.0, 200.0));
        assert_eq!(55.0 - card.origin.y, 15.0);
    }

    #[test]
    fn the_page_puts_no_terminal_in_its_window() {
        assert!(needs_a_terminal(&KeyAssignment::SpawnTab(
            SpawnTabDomain::CurrentPaneDomain
        )));
        assert!(!needs_a_terminal(
            &KeyAssignment::ActivateWorkspaceThreadByIndex(0)
        ));
        assert!(!needs_a_terminal(&KeyAssignment::ToggleWorkspaceSidebar));
    }
}
