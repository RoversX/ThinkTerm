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

use crate::frontend::front_end;
use crate::termwindow::content_view::{
    ContentView, ContentViewId, ContentViewResponse,
};
use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::{domain_is_attached, TermWindow, TermWindowNotif};
use crate::ui::{
    rect, ControlState, DrawContext, InteractionState, UiContext, UiPalette,
    WidgetKind,
};
use crate::workspace_threads::{self, WorkspaceThreadId};
use config::keyassignment::KeyAssignment;
use config::TermConfig;
use mux::pane::Pane;
use mux::window::WindowId as MuxWindowId;
use mux::Mux;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use wezterm_font::LoadedFont;
use wezterm_term::{KeyCode, KeyModifiers};
use window::{MouseEventKind as WMEK, MousePress, RectF, ScreenPoint, Window, WindowOps};

/// Keeps the page's mux window alive with no tab in it, whichever view is in
/// front of the page at the moment.
const THREAD_PAGE_SURFACE: &str = "thinkterm-thread-elsewhere";

const THREAD_PAGE_VIEW_KEY_PREFIX: &str = "thread-elsewhere:";

const HERO_ICON_SIZE: f32 = 76.0;
const BUTTON_MARGIN: f32 = 6.0;
const BUTTON_PADDING: f32 = 16.0;
const PAD: f32 = 32.0;

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
                workspace_threads::recorded_active_thread_for_space(&self.active_space_id)
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
            match return_to {
                Some(return_to) => self.activate_workspace_thread(return_to, window),
                None => self.show_thread_elsewhere_page(thread_id.clone()),
            }
        }
        // The reconcile must leave this one to us: it has a window coming.
        front_end().claim_spawned_mux_window(mux_window_id);
        let request = OwnWindowRequest {
            frame_slot: workspace_threads::note_thread_in_own_window(&thread_id),
            thread_id: thread_id.clone(),
            position,
        };
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

    pub(crate) fn focus_thread_window(&self, thread_id: &str) {
        if let Some(window) = front_end().thread_window(thread_id) {
            window.focus();
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
            self.activate_workspace_thread(thread_id.to_string(), &window);
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
            term_window.focus_thread_window(&thread_id);
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
        ctx.draw_rect(
            layers,
            0,
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            palette.window_bg,
        )?;

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
