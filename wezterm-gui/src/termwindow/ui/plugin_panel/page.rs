//! A plugin's page in a pane's place.
//!
//! The mux keeps a page's pane as it keeps a terminal's -- in a tab, in a
//! stack, beside others -- and says only which plugin's it is and what it
//! was opened with (`mux::pagepane`). What the page shows is drawn here, as
//! the plugin's panel in the sidebar is: a view the plugin host serves this
//! window, a player, and the same painting, in the pane's rectangle.
//!
//! A page is opened when its pane is first painted. Once a frame goes by
//! without it -- behind another tab, or under another of its stack -- it is
//! hidden: it keeps what it shows and what was typed in it, so that it is
//! there at once when it is looked at again, and costs nothing meanwhile.
//! Its recording is freed, nothing is asked of its plugin, and the host is
//! not told its last frame was taken in, so the host holds back whatever
//! its plugin draws next. A page is let go with its pane, and when more
//! than [`HIDDEN_PAGES`] are hidden, those looked at longest ago.
//!
//! The code that paints the sidebar's panel, takes its presses and edits
//! its fields works on the one panel a window has (`right_sidebar_plugin`).
//! A page is put in that place for as long as it is painted or handed an
//! event, and the sidebar's panel is put back after (`with_plugin_page`):
//! nothing of the two runs at once.

use super::{panel_env, panel_scale, tell_env, PluginPanel, Shown, Status};
use crate::plugins;
use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::{TermWindow, TermWindowNotif, UIItem, UIItemType};
use crate::utilsprites::RenderMetrics;
use anyhow::Context;
use config::keyassignment::KeyAssignment;
use mux::domain::DomainId;
use mux::pane::{Pane, PaneId};
use mux::tab::PositionedPane;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thinkterm_plugin_panel::{Player, Pointer, Terminal};
use thinkterm_proto::PageSpec;
use window::{MouseEvent, MouseEventKind as WMEK, WindowOps};

/// The most pages a window keeps while they are off show. One let go is
/// opened anew when its pane is next painted, as it was the first time.
const HIDDEN_PAGES: usize = 8;
/// How long a page stays open off show. Until then it comes back as it was
/// left, with no moment of starting; after, its plugin is told it went, and
/// runs on only as long as its own setting says a plugin unused does.
const HIDDEN_FOR: Duration = Duration::from_secs(120);
/// How long a page of another machine's plugin that lost its way there
/// waits before it looks for one again: nothing says when the machine is
/// reached anew.
const REMOTE_AGAIN: Duration = Duration::from_secs(3);

/// Whether `assignment` is about what a terminal holds -- its text, its
/// scrollback, what is typed into it -- of which a page has none: on a pane
/// that holds a page it does nothing, rather than search an empty screen
/// or send text nowhere. What is about the window, its tabs and its panes
/// is done as ever.
pub(crate) fn is_about_a_terminal(assignment: &KeyAssignment) -> bool {
    use KeyAssignment::*;
    matches!(
        assignment,
        Search(_)
            | ActivateCopyMode
            | QuickSelect
            | QuickSelectArgs(_)
            | CharSelect(_)
            | PasteFrom(_)
            | ClearScrollback(_)
            | ScrollByPage(_)
            | ScrollByLine(_)
            | ScrollToPrompt(_)
            | ScrollToTop
            | ScrollToBottom
            | SendString(_)
            | SendKey(_)
            | ResetTerminal
    )
}

/// Whether the panes of domain `domain_id` are kept on this machine -- in
/// this process, or by its session server -- so that a page among them is
/// this machine's plugin's to draw. Not whether their programs run here: a
/// WSL domain's pages are kept here as its panes are, whatever runs in its
/// terminals runs elsewhere.
pub(super) fn kept_here(domain_id: DomainId) -> bool {
    mux::Mux::get().get_domain(domain_id).is_some_and(|domain| {
        domain
            .downcast_ref::<wezterm_client::domain::ClientDomain>()
            .is_none_or(wezterm_client::domain::ClientDomain::is_local_session_host)
    })
}

/// The way to the server that keeps the panes of a domain of another
/// machine, for a link of plugin frames: none while the domain is
/// connecting, or connecting again.
fn way_to(domain_id: DomainId) -> Option<wezterm_client::plugin_link::PluginWay> {
    mux::Mux::get()
        .get_domain(domain_id)?
        .downcast_ref::<wezterm_client::domain::ClientDomain>()?
        .plugin_way()
}

/// Opens the view of page `held` again: with its machine's host, over a
/// link opened by `way` if none is open, for a page of another machine's
/// plugin.
fn reopen(held: &PluginPanel, way: Option<wezterm_client::plugin_link::PluginWay>) {
    match (held.remote, way, &held.page) {
        (Some(domain_id), Some(way), Some(page)) => plugins::reopen_remote_page(
            held.shown.view,
            domain_id,
            way,
            &held.plugin,
            held.shown.player.env(),
            page,
        ),
        // Looked for before this is asked: see `follow_plugin_page`.
        (Some(_), _, _) => {}
        (None, _, _) => held.reopen(),
    }
}

/// The terminal a page of domain `domain_id` was opened from -- pane `pane`
/// of the mux that keeps the page -- as `thinkterm cli` on the machine the
/// page's plugin runs on reaches it: by the session server's socket, or by
/// this process's; for another machine's page, by that machine's own.
pub(super) fn page_terminal(domain_id: DomainId, pane: u64) -> Terminal {
    let socket = if !kept_here(domain_id) {
        None
    } else if crate::local_sessions::is_host_domain_id(domain_id) {
        crate::local_sessions::candidate(&config::configuration())
            .map(|host| host.socket_path().to_string_lossy().into_owned())
    } else {
        crate::cli_socket().map(|socket| socket.to_string_lossy().into_owned())
    };
    Terminal { pane, socket }
}

/// A page of plugin `plugin`, one of this machine's that is on and has a
/// page, opened with `args`: named and marked as its plugin is. Its id,
/// and where the user is, the mux that keeps it gives it.
fn local_page(plugin: &str, args: &str) -> Option<PageSpec> {
    let known = plugins::plugins()
        .and_then(|all| all.into_iter().find(|info| info.id == plugin))
        .filter(|info| info.usable());
    let Some((name, page)) = known.and_then(|info| Some((info.name, info.page?))) else {
        log::warn!("no plugin named {plugin:?} has a page to open");
        return None;
    };
    Some(PageSpec {
        plugin: plugin.to_string(),
        args: args.to_string(),
        title: name,
        icon: page.icon,
        ..Default::default()
    })
}

impl TermWindow {
    /// Runs `with` while the page of pane `pane_id` stands where the
    /// sidebar's panel does. `None` when this window shows no such page.
    pub(crate) fn with_plugin_page<R>(
        &mut self,
        pane_id: PaneId,
        with: impl FnOnce(&mut Self) -> R,
    ) -> Option<R> {
        let page = self.plugin_pages.remove(&pane_id)?;
        let sidebar = self.right_sidebar_plugin.replace(page);
        // What is started here and ends later -- a paste -- finds the page
        // by its pane: by then it is back in its own place.
        let standing = self.plugin_page_standing.replace(pane_id);
        let result = with(self);
        self.plugin_page_standing = standing;
        // A page is not closed from under itself: what stands there now is
        // the page still.
        let page = std::mem::replace(&mut self.right_sidebar_plugin, sidebar);
        if let Some(page) = page {
            self.plugin_pages.insert(pane_id, page);
        }
        Some(result)
    }

    /// Opens the page of plugin `plugin` -- one of this machine's, on and
    /// with a page -- with `args`, in a tab of this window, beside the pane
    /// in focus and of its domain.
    pub(crate) fn open_plugin_page(&mut self, plugin: &str, args: &str) {
        // The plugins listed here are this machine's, and a page is drawn
        // by the plugin of the machine its pane is on: beside a pane of
        // another machine, a page of one of these would be another's.
        if !self.plugin_pages_open_here() {
            log::warn!("the page of plugin {plugin:?} is not opened beside a pane of another machine");
            return;
        }
        let Some(spec) = local_page(plugin, args) else {
            return;
        };
        let beside = self.get_active_pane_or_overlay().map(|pane| pane.pane_id());
        self.spawn_plugin_page(spec, beside);
    }

    /// Opens the page of plugin `plugin` with `args` beside pane `pane_id`,
    /// the page of that plugin that asked for it, and on that page's
    /// machine, wherever the focus is by now. Another machine's plugin is
    /// not listed here: the new page is named as the one that asked is,
    /// until its plugin names it.
    pub(super) fn open_plugin_page_beside(&mut self, pane_id: PaneId, plugin: &str, args: &str) {
        let Some(pane) = mux::Mux::get().get_pane(pane_id) else {
            return;
        };
        let spec = if kept_here(pane.domain_id()) {
            local_page(plugin, args)
        } else {
            pane.page().map(|asking| PageSpec {
                plugin: plugin.to_string(),
                args: args.to_string(),
                title: asking.title,
                icon: asking.icon,
                ..Default::default()
            })
        };
        if let Some(spec) = spec {
            self.spawn_plugin_page(spec, Some(pane_id));
        }
    }

    /// Opens page `spec` in a tab of this window, beside pane `beside` and
    /// of its domain.
    fn spawn_plugin_page(&self, spec: PageSpec, beside: Option<PaneId>) {
        let window_id = self.mux_window_id;
        let size = self.terminal_size;
        let mux = mux::Mux::get();
        let workspace = mux
            .get_window(window_id)
            .map(|window| window.get_workspace().to_string())
            .unwrap_or_else(|| mux.active_workspace());
        promise::spawn::spawn(async move {
            let plugin = spec.plugin.clone();
            let opened = mux::Mux::get()
                .spawn_page_tab(
                    Some(window_id),
                    config::keyassignment::SpawnTabDomain::CurrentPaneDomain,
                    spec,
                    size,
                    beside,
                    workspace,
                )
                .await;
            if let Err(err) = opened {
                log::error!("opening the page of plugin {plugin}: {err:#}");
            }
        })
        .detach();
    }

    /// Whether a page of one of this machine's plugins can be opened where
    /// the user is: the pane in focus runs on this machine, as does a tab
    /// opened beside it.
    pub(crate) fn plugin_pages_open_here(&self) -> bool {
        self.get_active_pane_or_overlay()
            .is_none_or(|pane| kept_here(pane.domain_id()))
    }

    /// The page the window's tab at `tab_idx` shows: the one its pane in
    /// focus holds, when that is a page.
    pub(crate) fn window_tab_page(&self, tab_idx: usize) -> Option<PageSpec> {
        let mux = mux::Mux::get();
        let window = mux.get_window(self.mux_window_id)?;
        window.get_by_idx(tab_idx)?.get_active_pane()?.page()
    }

    /// The pane whose page view `view` is, if it is a page's.
    pub(super) fn plugin_page_of_view(&self, view: u64) -> Option<PaneId> {
        self.plugin_pages
            .iter()
            .find(|(_, page)| page.shown.view == view)
            .map(|(pane_id, _)| *pane_id)
    }

    /// Paints the page `page` in the place of the pane at `pos`, opening it
    /// first if this window does not show it yet.
    pub(crate) fn paint_plugin_page(
        &mut self,
        pos: &PositionedPane,
        page: &PageSpec,
        layers: &mut TripleLayerQuadAllocator,
        bar_height: f32,
    ) -> anyhow::Result<()> {
        let pane_id = pos.pane.pane_id();
        // The room the pane's bar spans, from under the bar, which is
        // `bar_height` tall and painted already, to the foot of the pane's
        // ground. Not that ground's own span: beside an opaque sidebar it
        // goes on under the sidebar.
        let ground = self.pane_background_rect(pos);
        let (left, span) = self.pane_chrome_span(pos)?;
        let (_, top) = self.pane_content_origin(pos)?;
        let top = top + bar_height;
        let x = left.max(0.0) as usize;
        let y = top.max(0.0) as usize;
        let width = span.max(0.0) as usize;
        let height = (ground.max_y() - top).max(0.0) as usize;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let chrome = self.chrome();
        let settings = crate::native_settings::load_shared();
        let ui_font = self
            .fonts
            .title_font_with_size(crate::native_settings::right_sidebar_font_size(&settings))
            .context("plugin page ui font")?;
        let ui_metrics = RenderMetrics::with_font_metrics(&ui_font.metrics());
        let fonts = self.plugin_panel_fonts()?;
        let scale = panel_scale(self.dimensions.dpi);

        // What the plugin draws its own ground with is the sidebar's: the
        // page stands on that, whatever the terminals beside it stand on.
        let ground = self.sidebar_surface(chrome.workspace_sidebar_bg);
        self.filled_rectangle(
            layers,
            0,
            euclid::rect(x as f32, y as f32, width as f32, height as f32),
            ground,
        )?;

        self.plugin_pages_on_show.insert(pane_id);
        // A page is drawn by the plugin of the machine whose mux keeps its
        // pane: this machine's host, or, over a link of its own to that
        // machine's server, the host there -- never a plugin of the same
        // name on this machine drawing something else in its place.
        let domain_id = pos.pane.domain_id();
        let remote = (!kept_here(domain_id)).then_some(domain_id);
        let way = remote.and_then(way_to);
        // Its machine is being connected to: the page starts once it is,
        // looked for again in a while.
        if remote.is_some() && way.is_none() && !self.plugin_pages.contains_key(&pane_id) {
            self.update_next_frame_time(Some(Instant::now() + REMOTE_AGAIN));
            let inset = (12.0 * scale) as usize;
            return self.paint_sidebar_text(
                layers,
                &ui_font,
                ui_metrics,
                &crate::i18n::tr("right-plugin-starting"),
                x + inset,
                y + inset,
                width.saturating_sub(inset * 2),
                chrome.secondary_text,
            );
        }

        let mut env = panel_env(&fonts, &chrome, width as f32, height as f32, scale);
        env.pointer = Pointer::Mouse;
        // Where the user was when the page was opened, as the machine its
        // plugin runs on knows it.
        (env.cwd, env.terminal) = self.plugin_page_origin(pane_id, page, domain_id);
        self.follow_plugin_page(pane_id, page, env, remote, way);
        self.ui_items.push(UIItem {
            x,
            y,
            width,
            height,
            item_type: UIItemType::PluginPage(pane_id),
        });

        let Some(mut held) = self.plugin_pages.remove(&pane_id) else {
            return Ok(());
        };
        // The keyboard is the page's only while its pane is the one in
        // focus and no field of the sidebar's has what is typed: given up
        // before it is painted, so that no caret shows in it and the system
        // is not left editing its text.
        if (!pos.is_active || self.right_sidebar_has_text_focus())
            && held.shown.player.has_keyboard()
        {
            held.shown.player.blur();
            held.shown.tell();
            self.forget_plugin_field_snapshot_of(held.shown.view);
        }
        let painted = self.paint_plugin_view(
            layers,
            &mut held.shown,
            false,
            (x, y, width),
            scale,
            &ui_font,
            ui_metrics,
            &fonts,
            &chrome,
        );
        self.plugin_pages.insert(pane_id, held);
        painted
    }

    /// Where page `page`, of pane `pane_id` of domain `domain_id`, was
    /// opened from, as its view's `Env` says: the directory, and the
    /// terminal as [`page_terminal`] reaches it. Worked out once for each
    /// opening, and taken from its view after.
    fn plugin_page_origin(
        &self,
        pane_id: PaneId,
        page: &PageSpec,
        domain_id: DomainId,
    ) -> (Option<String>, Option<Terminal>) {
        let held = self.plugin_pages.get(&pane_id).filter(|held| {
            held.page
                .as_ref()
                .is_some_and(|was| was.id == page.id && was.terminal == page.terminal)
        });
        if let Some(held) = held {
            let env = held.shown.player.env();
            return (env.cwd.clone(), env.terminal.clone());
        }
        let cwd = (!page.cwd.is_empty()).then(|| page.cwd.clone());
        let terminal = page.terminal.map(|pane| page_terminal(domain_id, pane));
        (cwd, terminal)
    }

    /// Hands the mux what the plugin of the page of pane `pane_id` changed
    /// of it in the frame just heard -- its title, its icon, what it opens
    /// with: the mux keeps the page, and tells every client.
    pub(super) fn follow_plugin_page_change(&mut self, pane_id: PaneId) {
        let Some(change) = self
            .plugin_pages
            .get_mut(&pane_id)
            .and_then(|held| held.shown.player.take_page_change())
        else {
            return;
        };
        let Some(pane) = mux::Mux::get().get_pane(pane_id) else {
            return;
        };
        let edit = thinkterm_proto::PageEdit {
            title: change.title,
            icon: change.icon,
            args: change.args.map(|args| args.to_string()),
        };
        // Sent whatever the page is here: for another machine's page that
        // is what its server last said, which a change made just before
        // this one has not reached yet. The mux drops one that changes
        // nothing.
        if let Err(err) = pane.change_page(edit) {
            log::warn!("the page of pane {pane_id} is not changed as its plugin asked: {err:#}");
        }
    }

    /// Opens the page of pane `pane_id` for `env` unless this window shows
    /// it already; tells the plugin a size or theme that changed, as a
    /// panel's is told; and opens one the host closed again once it is time.
    fn follow_plugin_page(
        &mut self,
        pane_id: PaneId,
        page: &PageSpec,
        env: thinkterm_plugin_panel::Env,
        remote: Option<DomainId>,
        way: Option<wezterm_client::plugin_link::PluginWay>,
    ) {
        let now = Instant::now();
        let due = match self.plugin_pages.get_mut(&pane_id) {
            // Another page in the pane's place: the pane's id was given to
            // another, or what it holds is another page.
            Some(held)
                if held.plugin != page.plugin
                    || held.page.as_ref().is_none_or(|was| was.id != page.id)
                    || held.remote != remote =>
            {
                self.close_plugin_page(pane_id);
                return self.follow_plugin_page(pane_id, page, env, remote, way);
            }
            Some(held) => {
                held.last_shown = now;
                // The same page, which its plugin changed: what it opens
                // with now is what it is opened with again.
                if held.page.as_ref() != Some(page) {
                    held.page = Some(page.clone());
                }
                // The plugins were listed anew since this was looked at: one
                // the host would not serve -- off, or not yet let run -- may
                // be served now.
                let listed = plugins::actions_heard();
                let listed_anew = std::mem::replace(&mut held.listed, listed) != listed;
                let shown = &mut held.shown;
                // Back on show: the host hears its last frame was taken in,
                // and sends what its plugin drew since.
                let back = std::mem::take(&mut shown.hidden);
                if back {
                    if std::mem::take(&mut shown.ack_owed) {
                        plugins::tell_panel(
                            shown.view,
                            thinkterm_plugin_channel::wire::PanelRequest::Shown,
                        );
                    }
                    super::send_wanted(shown);
                }
                // One the host stopped serving is asked for again when it
                // comes back on show, and when the plugins changed.
                if matches!(shown.status, Status::Stopped(_)) && (back || listed_anew) {
                    shown.status = Status::Again(now);
                }
                // Another machine's, whose link ended: nothing says when
                // the machine is reached anew, so it is opened again in a
                // while, showing what it last drew meanwhile.
                if held.remote.is_some() {
                    if let Status::Lost(why) = &shown.status {
                        log::info!("the page of plugin {} lost its machine: {why}", held.plugin);
                        shown.status = Status::Again(now + REMOTE_AGAIN);
                    }
                }
                match shown.status {
                    // No way to its machine for the moment: looked for
                    // again in a while.
                    Status::Again(at) if now >= at && held.remote.is_some() && way.is_none() => {
                        let at = now + REMOTE_AGAIN;
                        shown.status = Status::Again(at);
                        Some(at)
                    }
                    Status::Again(at) if now >= at => {
                        shown.player.set_env(env);
                        shown.player.restarted();
                        shown.status = Status::Opening;
                        shown.env_told = now;
                        shown.env_due = false;
                        held.opening = held.opening.wrapping_add(1);
                        reopen(held, way);
                        None
                    }
                    Status::Again(at) => Some(at),
                    _ => tell_env(shown, env, now),
                }
            }
            None => {
                let Some(window) = self.window.clone() else {
                    return;
                };
                let view = match (remote, way) {
                    (Some(domain_id), Some(way)) => {
                        plugins::open_remote_page(domain_id, way, &page.plugin, &env, page, &window)
                    }
                    // With no way there it is not opened: see the caller.
                    (Some(_), None) => return,
                    (None, _) => plugins::open_page(&page.plugin, &env, page, &window),
                };
                let mut held = PluginPanel::page(&page.plugin, page, Shown::new(view, Player::new(env)));
                held.remote = remote;
                self.plugin_pages.insert(pane_id, held);
                None
            }
        };
        if let Some(due) = due {
            self.update_next_frame_time(Some(due));
        }
    }

    /// Lets go of the page of pane `pane_id`: its plugin is told, and what
    /// it held is freed.
    pub(crate) fn close_plugin_page(&mut self, pane_id: PaneId) {
        if let Some(page) = self.plugin_pages.remove(&pane_id) {
            self.forget_plugin_field_snapshot_of(page.shown.view);
            plugins::close_panel(page.shown.view);
        }
    }

    /// Lets go of every page this window shows: the window is going.
    pub(crate) fn close_plugin_pages(&mut self) {
        let panes: Vec<PaneId> = self.plugin_pages.keys().copied().collect();
        for pane_id in panes {
            self.close_plugin_page(pane_id);
        }
    }

    /// Lets go of the pages that have been off show for [`HIDDEN_FOR`]:
    /// looked at when one is hidden, and that long after.
    fn close_plugin_pages_long_hidden(&mut self) {
        let long: Vec<PaneId> = self
            .plugin_pages
            .iter()
            .filter(|(_, page)| page.shown.hidden && page.last_shown.elapsed() >= HIDDEN_FOR)
            .map(|(pane_id, _)| *pane_id)
            .collect();
        for pane_id in long {
            self.close_plugin_page(pane_id);
        }
    }

    /// Hides every page that was not painted since this was last called,
    /// and lets go of those whose pane is gone, and of the ones hidden
    /// longest once there are more than [`HIDDEN_PAGES`].
    pub(crate) fn let_go_of_plugin_pages_off_show(&mut self) {
        let on_show = std::mem::take(&mut self.plugin_pages_on_show);
        if self.plugin_pages.is_empty() {
            return;
        }
        let mux = mux::Mux::get();
        let mut hidden = Vec::new();
        let mut gone = Vec::new();
        let mut newly_hidden = Vec::new();
        for (pane_id, page) in self.plugin_pages.iter_mut() {
            if on_show.contains(pane_id) {
                continue;
            }
            if mux.get_pane(*pane_id).is_none() {
                gone.push(*pane_id);
                continue;
            }
            if !page.shown.hidden {
                page.shown.hidden = true;
                // What it was painted with is made again when it is seen.
                page.shown.painted = None;
                page.shown.player.pointer_left();
                page.selecting = None;
                // The keyboard is not a page's that cannot be seen.
                if page.shown.player.has_keyboard() {
                    page.shown.player.blur();
                    page.shown.tell();
                }
                newly_hidden.push(page.shown.view);
            }
            hidden.push((page.last_shown, *pane_id));
        }
        for view in &newly_hidden {
            // The system is not left editing the text of a field out of
            // sight: it would take what is typed at a terminal for it.
            self.forget_plugin_field_snapshot_of(*view);
        }
        if !newly_hidden.is_empty() {
            self.close_plugin_pages_long_hidden_later();
        }
        self.close_plugin_pages_long_hidden();
        if hidden.len() > HIDDEN_PAGES {
            hidden.sort_unstable();
            let over = hidden.len() - HIDDEN_PAGES;
            gone.extend(hidden.into_iter().take(over).map(|(_, pane_id)| pane_id));
        }
        for pane_id in gone {
            self.close_plugin_page(pane_id);
        }
    }

    /// The pointer or the wheel over the page of pane `pane_id`.
    pub(crate) fn mouse_event_plugin_page(
        &mut self,
        pane_id: PaneId,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let wheel = matches!(event.kind, WMEK::VertWheel(_) | WMEK::HorzWheel(_));
        if !wheel {
            // A press in a page puts its pane in focus, as one in a terminal
            // does, and takes the keys from a field of the sidebar.
            if matches!(event.kind, WMEK::Press(_)) {
                self.focus_plugin_page_pane(pane_id);
                if self.right_sidebar_has_text_focus() {
                    self.clear_right_sidebar_text_focus();
                }
                context.invalidate();
            }
            self.with_plugin_page(pane_id, |this| {
                this.mouse_event_plugin_panel(event, context, false)
            });
            return;
        }
        // Across a list wider than the page: the trackpad's own, or the
        // wheel with Shift.
        let shift_vertical = matches!(event.kind, WMEK::VertWheel(_))
            && event.modifiers.contains(::window::Modifiers::SHIFT);
        let (dx, dy) = match event.kind {
            WMEK::HorzWheel(_) => (self.sidebar_horizontal_scroll_delta(&event), None),
            WMEK::VertWheel(_) if shift_vertical => {
                (self.sidebar_vertical_scroll_delta(&event), None)
            }
            _ => (None, self.sidebar_vertical_scroll_delta(&event)),
        };
        let (dx, dy) = (dx.unwrap_or(0.0), dy.unwrap_or(0.0));
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let moved = self.with_plugin_page(pane_id, |this| this.plugin_panel_wheel(&event, dx, dy));
        if moved == Some(true) {
            context.invalidate();
        }
    }

    /// Makes pane `pane_id` the one in focus in its tab, if the tab on show
    /// has it and another has the focus.
    fn focus_plugin_page_pane(&mut self, pane_id: PaneId) {
        let Some(tab) = mux::Mux::get().get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        let panes = tab.iter_panes();
        let Some(pos) = panes.iter().find(|pos| pos.pane.pane_id() == pane_id) else {
            return;
        };
        if !pos.is_active {
            tab.set_active_idx(pos.index);
        }
    }

    /// The pane `pane`, when it holds a page this window has on show.
    pub(crate) fn plugin_page_in_focus(&self, pane: &Arc<dyn Pane>) -> Option<PaneId> {
        let pane_id = pane.pane_id();
        let on_show = self
            .plugin_pages
            .get(&pane_id)
            .is_some_and(|page| !page.shown.hidden);
        (on_show && pane.is_page()).then_some(pane_id)
    }

    /// Comes back once the pages hidden now have been off show for
    /// [`HIDDEN_FOR`], to let go of those still hidden: nothing else is
    /// sure to look -- a window nobody touches paints nothing.
    fn close_plugin_pages_long_hidden_later(&self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        promise::spawn::spawn(async move {
            smol::Timer::after(HIDDEN_FOR + Duration::from_secs(1)).await;
            window.notify(TermWindowNotif::Apply(Box::new(|term_window| {
                term_window.close_plugin_pages_long_hidden();
            })));
        })
        .detach();
    }

    /// Pastes the clipboard into the field with the keyboard of the page
    /// of pane `pane_id`: once the clipboard answers, and only while the
    /// page is still where what is typed goes -- its pane in focus, no
    /// field of the sidebar's and no view over it having the keys.
    pub(crate) fn paste_into_plugin_page(
        &mut self,
        pane_id: PaneId,
        source: config::keyassignment::ClipboardPasteSource,
    ) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let clipboard = window.get_clipboard(match source {
            config::keyassignment::ClipboardPasteSource::Clipboard => ::window::Clipboard::Clipboard,
            config::keyassignment::ClipboardPasteSource::PrimarySelection => {
                ::window::Clipboard::PrimarySelection
            }
        });
        promise::spawn::spawn(async move {
            if let Ok(text) = clipboard.await {
                window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    let still_there = term_window
                        .get_active_pane_or_overlay()
                        .and_then(|pane| term_window.plugin_page_in_focus(&pane))
                        == Some(pane_id)
                        && !term_window.right_sidebar_has_text_focus()
                        && !term_window.content_view_foreground();
                    if still_there && term_window.plugin_page_text(pane_id, &text) {
                        term_window.invalidate_window();
                    }
                })));
            }
        })
        .detach();
    }

    /// Copies what is selected in the field with the keyboard of the page
    /// of pane `pane_id`, as a panel's is copied.
    pub(crate) fn copy_from_plugin_page(
        &mut self,
        pane_id: PaneId,
        destination: config::keyassignment::ClipboardCopyDestination,
    ) {
        self.with_plugin_page(pane_id, |this| this.plugin_panel_copy(destination));
    }

    /// A key pressed while the pane in focus holds the page of pane
    /// `pane_id`: the field with the keyboard edits, and what it does not
    /// use goes to the page's player, for its plugin when it takes that
    /// key. True when the page took it.
    pub(crate) fn plugin_page_key(
        &mut self,
        pane_id: PaneId,
        key: ::termwiz::input::KeyCode,
        mods: ::termwiz::input::Modifiers,
    ) -> bool {
        self.with_plugin_page(pane_id, |this| {
            this.give_plugin_page_the_keyboard();
            this.plugin_panel_key(key, mods)
        })
        .unwrap_or(false)
    }

    /// Text typed or composed while the pane in focus holds the page of
    /// pane `pane_id`: for the field with the keyboard.
    pub(crate) fn plugin_page_text(&mut self, pane_id: PaneId, text: &str) -> bool {
        self.with_plugin_page(pane_id, |this| {
            this.give_plugin_page_the_keyboard();
            this.plugin_panel_text(text)
        })
        .unwrap_or(false)
    }

    /// The page standing in the panel's place has the keyboard: the user
    /// put the pane in focus, which is their giving it.
    fn give_plugin_page_the_keyboard(&mut self) {
        if let Some(page) = self.right_sidebar_plugin.as_mut() {
            if !page.shown.player.has_keyboard() {
                page.shown.player.focus_panel();
                page.shown.tell();
            }
        }
    }

    /// The pointer left the page of pane `pane_id`.
    pub(crate) fn plugin_page_pointer_left(&mut self, pane_id: PaneId) -> bool {
        self.with_plugin_page(pane_id, |this| this.plugin_panel_pointer_left(false))
            .unwrap_or(false)
    }
}
