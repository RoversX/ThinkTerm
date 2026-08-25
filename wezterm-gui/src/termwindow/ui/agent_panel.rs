//! The right-sidebar Agents panel: lists panes running detected coding
//! agents with their live state. Painted only while the agent-panel
//! feature toggle is on; all data comes from `crate::agent_status`
//! snapshots — the paint path never triggers detection or filesystem
//! probes.

use crate::agent_status::{self, AgentPanelAction, AgentState};
use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::ui::right_sidebar::{
    right_sidebar_file_row_metrics, sidebar_row_element_visible, FILE_SCROLL_FADE_HEIGHT,
};
use crate::termwindow::ui::tokens::{SIDEBAR_INSET, SIDEBAR_ROW_RADIUS};
use crate::termwindow::{TermWindow, TermWindowNotif, UIItem, UIItemType};
use crate::ui::UiPalette;
use crate::utilsprites::RenderMetrics;
use anyhow::Context;
use mux::pane::PaneId;
use mux::Mux;
use std::rc::Rc;
use wezterm_client::domain::FrontendRecoverySlot;
use wezterm_font::LoadedFont;
use window::color::LinearRgba;
use window::{MouseEvent, MouseEventKind as WMEK, MousePress, WindowOps};

impl TermWindow {
    /// Height of the strip above the list: the reload button, the optional
    /// status line, and the gap before the first row. The scroll mask covers
    /// exactly this, so the two must be derived from the same numbers.
    fn agents_toolbar_height(&self, cell_height: usize, has_status: bool) -> usize {
        let status_height = if has_status {
            self.ui_px(6) + cell_height
        } else {
            0
        };
        // This strip doubles as the top scroll mask, and it only has to
        // swallow a single element's overflow -- the card behind them is
        // clipped geometrically -- so the ordinary gap is already tall enough.
        self.agents_toolbar_button_height(cell_height) + status_height + self.ui_px(8)
    }

    /// Two lines -- identity, then state and place -- with the same share of
    /// vertical padding a file row gives its single line.
    fn agents_row_height(&self, cell_height: usize) -> (usize, usize) {
        let line_gap = (cell_height * 3 / 10).max(1);
        let padding = (cell_height * 7 / 10).max(1);
        (cell_height * 2 + line_gap + padding, line_gap)
    }

    fn agents_toolbar_button_height(&self, cell_height: usize) -> usize {
        (cell_height + self.ui_px(12)).max(self.ui_px(28))
    }

    /// Painted on layer 2 *after* the rows: the rows are drawn unclipped and a
    /// mask erases whatever overflows above the list, so the toolbar has to go
    /// back on top of that mask.
    #[allow(clippy::too_many_arguments)]
    fn paint_agents_toolbar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        status: Option<&str>,
    ) -> anyhow::Result<()> {
        let cell_height = ui_metrics.cell_size.height as usize;
        let button_height = self.agents_toolbar_button_height(cell_height);
        self.paint_snippet_button(
            layers,
            2,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            content_x,
            content_top,
            content_width,
            button_height,
            Some(SvgIcon::RotateCw),
            &crate::i18n::tr("right-agents-reload-rules"),
            UIItemType::RightSidebarAgent(AgentPanelAction::ReloadRules),
            true,
        )?;
        if let Some(status) = status {
            // The acknowledgement expires by TTL, but expiry alone paints
            // nothing; keep a lazy repaint scheduled while it is visible.
            self.update_next_frame_time(Some(
                std::time::Instant::now() + std::time::Duration::from_millis(500),
            ));
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                status,
                content_x + self.ui_px(SIDEBAR_INSET),
                content_top + button_height + self.ui_px(6),
                content_width.saturating_sub(self.ui_px(SIDEBAR_INSET) * 2),
                muted_fg,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_agents_sidebar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
    ) -> anyhow::Result<()> {
        let cell_height = ui_metrics.cell_size.height as usize;
        let (row_height, line_gap) = self.agents_row_height(cell_height);
        // Read once: the status expires on a TTL, and asking twice could size
        // the toolbar for one layout and paint another.
        let status = agent_status::panel_status();
        let toolbar_height = self.agents_toolbar_height(cell_height, status.is_some());
        let list_top = content_top + toolbar_height;
        let inset = self.ui_px(SIDEBAR_INSET);

        let mut agents = agent_status::list_agent_panes();
        if agents.is_empty() {
            let card_height = (cell_height * 2 + self.ui_px(24))
                .min(content_bottom.saturating_sub(list_top + inset));
            if card_height > 0 {
                self.fill_rounded_rectangle_with_border(
                    layers,
                    1,
                    euclid::rect(
                        content_x as f32,
                        list_top as f32,
                        content_width as f32,
                        card_height as f32,
                    ),
                    chrome.sidebar_button_bg,
                    chrome.control_border.mul_alpha(0.74),
                    self.ui_f32(SIDEBAR_ROW_RADIUS) + 6.0,
                    1.0,
                )
                .context("agents empty state card")?;
                let pad = inset * 2;
                self.paint_sidebar_text(
                    layers,
                    ui_font,
                    ui_metrics,
                    &crate::i18n::tr("right-agents-empty"),
                    content_x + pad,
                    list_top + self.ui_px(10),
                    content_width.saturating_sub(pad * 2),
                    foreground,
                )?;
                self.paint_sidebar_text(
                    layers,
                    ui_font,
                    ui_metrics,
                    &crate::i18n::tr("right-agents-empty-hint"),
                    content_x + pad,
                    list_top + self.ui_px(12) + cell_height,
                    content_width.saturating_sub(pad * 2),
                    muted_fg,
                )?;
            }
            return self.paint_agents_toolbar(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                content_width,
                status.as_deref(),
            );
        }

        // Rows for the current window's workspace first, then stable order.
        let current_workspace = Mux::get().active_workspace();
        agents.sort_by(|a, b| {
            let a_here = a.workspace == current_workspace;
            let b_here = b.workspace == current_workspace;
            b_here
                .cmp(&a_here)
                .then_with(|| a.agent_id.cmp(&b.agent_id))
                .then_with(|| a.pane_id.cmp(&b.pane_id))
        });

        // Borrow the file tree's row chrome wholesale so the two lists read as
        // one control: same height, same icon size, same gaps, all derived from
        // the font instead of from physical-pixel constants. Rows sit flush
        // against each other -- no gap, no card, no border.
        let metrics = right_sidebar_file_row_metrics(ui_metrics);
        // The panel's bottom padding, which doubles as the mask that keeps
        // rows out of it. Whatever overshoots it runs past the panel, where
        // the window edge cuts it -- unless a tab bar sits at the bottom:
        // its background is painted on layer 0 while these glyphs are on
        // layer 2, and layer index beats paint order, so overflow would land
        // *on top of* the bar. In that configuration the panel bottom
        // (already the tab bar's top edge, see right_sidebar_rect) must
        // bound the rows instead.
        let viewport_bottom = content_bottom.saturating_sub(inset * 2);
        let masked_below = self.show_tab_bar && self.config.tab_bar_at_bottom;
        let overflow_bottom = if masked_below {
            content_bottom
        } else {
            usize::MAX
        };
        // Unmasked, rows run to the window edge and are cut there; their
        // hover chrome must reach as far, or glyphs outrun their tint.
        let row_clip_bottom = if masked_below {
            viewport_bottom
        } else {
            content_bottom
        };
        // An element is painted while it straddles an edge, because something
        // cuts it: the header mask above, the window edge below. That is the
        // whole difference between this and withholding it until it fits,
        // which is what made the list step.
        let visible = |elem_y: usize, elem_height: usize| {
            sidebar_row_element_visible(elem_y, elem_height, content_top, list_top, overflow_bottom)
        };
        let visible_height = viewport_bottom.saturating_sub(list_top);
        let total_height = agents.len().saturating_mul(row_height);
        let max_scroll = total_height.saturating_sub(visible_height) as f32;
        self.right_sidebar_agents_scroll = self.right_sidebar_agents_scroll.clamp(0.0, max_scroll);
        let scroll = self.right_sidebar_agents_scroll;

        for (idx, agent) in agents.iter().enumerate() {
            let row_top = list_top as f32 + (idx * row_height) as f32 - scroll;
            if row_top + row_height as f32 <= list_top as f32 {
                continue;
            }
            if row_top >= viewport_bottom as f32 {
                // Rows are laid out top-down, so nothing below is visible.
                break;
            }
            let Some(band) =
                agent_row_visible_band(row_top, row_height, list_top, row_clip_bottom)
            else {
                continue;
            };
            let top = band.top;
            // Only the hover tint is clipped -- it is the one thing that would
            // look wrong bleeding past the list, since it reads as a control.
            let hovered = self.is_pointer_over_ui_rect(
                content_x,
                band.visible_y,
                content_width,
                band.visible_height,
            );
            if hovered {
                self.fill_rounded_rectangle(
                    layers,
                    1,
                    euclid::rect(
                        content_x as f32,
                        band.visible_y as f32,
                        content_width as f32,
                        band.visible_height as f32,
                    ),
                    chrome.sidebar_button_hover_bg,
                    self.ui_f32(SIDEBAR_ROW_RADIUS),
                )
                .context("agent row hover")?;
            }
            let in_this_window = agent.window_id == Some(self.mux_window_id);
            self.ui_items.push(UIItem {
                x: content_x,
                y: band.visible_y,
                width: content_width,
                height: band.visible_height,
                item_type: UIItemType::RightSidebarAgent(if in_this_window {
                    AgentPanelAction::Reveal(agent.pane_id)
                } else {
                    AgentPanelAction::RevealElsewhere(agent.pane_id)
                }),
            });

            let icon_x = content_x + inset;
            let icon_y = top + (row_height.saturating_sub(metrics.icon_size)) / 2;
            if visible(icon_y, metrics.icon_size) {
                self.paint_sidebar_icon(
                    layers,
                    SvgIcon::Bot,
                    icon_x,
                    icon_y,
                    metrics.icon_size,
                    muted_fg,
                )?;
            }

            // State chip: spinner while working, alert while blocked. `None`
            // reserves no width on the right either, so an Unknown agent gets
            // the whole line for its title.
            let state_size = metrics.icon_size;
            let state_x = content_x + content_width - inset - state_size;
            let state_y = top + (row_height.saturating_sub(state_size)) / 2;
            let state_icon = match agent.state {
                AgentState::Working => Some((SvgIcon::LoaderCircle, foreground, true)),
                AgentState::Blocked => Some((SvgIcon::CircleAlert, foreground, false)),
                AgentState::Idle => Some((SvgIcon::CircleCheck, muted_fg, false)),
                AgentState::Unknown => None,
            };
            let has_state_icon = state_icon.is_some();
            if let Some((icon, color, spinning)) = state_icon.filter(|_| visible(state_y, state_size))
            {
                if spinning {
                    self.paint_spinning_ui_icon(
                        layers,
                        2,
                        icon,
                        state_x,
                        state_y,
                        state_size,
                        color,
                    )?;
                } else {
                    self.paint_sidebar_icon(layers, icon, state_x, state_y, state_size, color)?;
                }
            }

            let text_x = icon_x + metrics.icon_size + metrics.icon_gap;
            let text_right = if has_state_icon {
                state_x.saturating_sub(metrics.icon_gap)
            } else {
                content_x + content_width - inset
            };
            let text_width = text_right.saturating_sub(text_x);
            let title_y = top + (row_height.saturating_sub(cell_height * 2 + line_gap)) / 2;
            let name = agent_status::display_name(&agent.agent_id);
            let title_line = if agent.title.is_empty() {
                name
            } else {
                format!("{name} · {}", agent.title)
            };
            if visible(title_y, cell_height) {
                self.paint_sidebar_text(
                    layers,
                    ui_font,
                    ui_metrics,
                    &title_line,
                    text_x,
                    title_y,
                    text_width,
                    foreground,
                )?;
            }
            let state_label = match agent.state {
                AgentState::Working => crate::i18n::tr("right-agents-state-working"),
                AgentState::Blocked => crate::i18n::tr("right-agents-state-blocked"),
                AgentState::Idle => crate::i18n::tr("right-agents-state-idle"),
                AgentState::Unknown => crate::i18n::tr("right-agents-state-unknown"),
            };
            // The thread's human name, resolved once per snapshot -- never the
            // internal workspace id, and never a per-frame store walk.
            let detail_line = if agent.place.is_empty() {
                state_label
            } else {
                format!("{state_label} · {}", agent.place)
            };
            let detail_y = title_y + cell_height + line_gap;
            if visible(detail_y, cell_height) {
                self.paint_sidebar_text(
                    layers,
                    ui_font,
                    ui_metrics,
                    &detail_line,
                    text_x,
                    detail_y,
                    text_width,
                    muted_fg,
                )?;
            }
        }

        let scrolled = max_scroll > 0.0 && scroll > 0.0;
        // Only a bottom tab bar needs protecting from row overflow; with
        // the window edge below, rows run to it and are cut there.
        if max_scroll > 0.0 && masked_below {
            self.paint_right_sidebar_file_mask(
                layers,
                chrome,
                content_x,
                viewport_bottom,
                content_width,
                content_bottom.saturating_sub(viewport_bottom),
            )?;
        }
        // Above the list sits the toolbar strip and nothing paints over it
        // afterwards: repaint it opaque, put the toolbar back on top, then
        // soften the cut.
        if scrolled {
            self.paint_right_sidebar_file_mask(
                layers,
                chrome,
                content_x,
                content_top,
                content_width,
                toolbar_height,
            )?;
        }
        self.paint_agents_toolbar(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            content_x,
            content_top,
            content_width,
            status.as_deref(),
        )?;
        if scrolled {
            let fade_height = self
                .ui_px(FILE_SCROLL_FADE_HEIGHT)
                .min(viewport_bottom.saturating_sub(list_top));
            self.paint_right_sidebar_file_top_fade(
                layers,
                chrome,
                content_x,
                list_top,
                content_width,
                fade_height,
            )?;
        }

        Ok(())
    }

    pub(crate) fn mouse_event_right_sidebar_agent(
        &mut self,
        item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let UIItemType::RightSidebarAgent(action) = item.item_type else {
            return;
        };
        context.set_cursor(Some(window::MouseCursor::Hand));
        if event.kind != WMEK::Press(MousePress::Left) {
            return;
        }
        match action {
            AgentPanelAction::ReloadRules => {
                // The reload reads manifest files; keep that off the GUI
                // thread and report completion when it lands.
                std::thread::spawn(|| {
                    mux::agent_status::reload_rules();
                    promise::spawn::spawn_into_main_thread(async {
                        agent_status::set_panel_status(crate::i18n::tr(
                            "right-agents-rules-reloaded",
                        ));
                        if let Some(front_end) = crate::frontend::try_front_end() {
                            front_end.invalidate_all_windows();
                        }
                    })
                    .detach();
                });
            }
            AgentPanelAction::Reveal(pane_id) => {
                self.reveal_agent_pane(pane_id);
            }
            AgentPanelAction::RevealElsewhere(pane_id) => {
                self.reveal_agent_pane_elsewhere(pane_id, context);
            }
        }
        context.invalidate();
    }

    /// The agent lives in another window: deep-focus its tab and pane in
    /// the owning mux window first, then activate the backing thread —
    /// the same path a left-sidebar thread click takes, which switches
    /// the workspace and brings the right window forward. The activated
    /// window then comes up already pointing at the agent.
    fn reveal_agent_pane_elsewhere(&mut self, pane_id: PaneId, context: &dyn WindowOps) {
        let mux = Mux::get();
        let Some((_domain, window_id, tab_id)) = mux.resolve_pane_id(pane_id) else {
            return;
        };
        let tab_idx = mux.get_window(window_id).and_then(|window| {
            (0..window.len()).find(|idx| {
                window
                    .get_by_idx(*idx)
                    .map(|tab| tab.tab_id() == tab_id)
                    .unwrap_or(false)
            })
        });
        if let Some(idx) = tab_idx {
            if let Some(mut window) = mux.get_window_mut(window_id) {
                window.save_and_then_set_active(idx);
            }
        }
        if let (Some(tab), Some(pane)) = (mux.get_tab(tab_id), mux.get_pane(pane_id)) {
            tab.set_active_pane(&pane);
        }
        let Some(workspace) = mux
            .get_window(window_id)
            .map(|window| window.get_workspace().to_string())
        else {
            return;
        };
        if let Some(thread_id) =
            crate::workspace_threads::thread_id_for_workspace_any(&workspace)
        {
            // Only a thread of this Space (or one it references) can be
            // activated here; activate_workspace_thread's guard silently
            // drops anything else, which used to make these clicks no-ops.
            let home_space = crate::workspace_threads::thread_space_id(&thread_id);
            let in_this_space =
                home_space.as_deref() == Some(self.active_space_id.as_str());
            let referenced = !in_this_space
                && crate::workspace_threads::thread_ref_exists(
                    &self.active_space_id,
                    &thread_id,
                );
            if in_this_space || referenced {
                self.activate_workspace_thread(thread_id, context);
                return;
            }
            let Some(home_space) = home_space else {
                return;
            };
            // The thread belongs to another Space. Hand the activation to
            // the window owning that Space, inside that window's own event
            // loop -- the mux-window lookup would only find a window that
            // is already displaying this very thread, which is exactly the
            // case that cannot happen here. Same shape as live_overview's
            // open_thread_response.
            if let Some(target_owner_id) =
                crate::workspace_threads::window_owner_for_space(&home_space)
            {
                let Some(target) = crate::frontend::front_end()
                    .gui_window_for_recovery_slot(FrontendRecoverySlot::Window(
                        target_owner_id,
                    ))
                else {
                    return;
                };
                let target_window = target.window.clone();
                target.window.notify(TermWindowNotif::Apply(Box::new(
                    move |target_term_window| {
                        // Ownership can change between the click and this
                        // callback; do not steal a Space on a stale lookup.
                        if crate::workspace_threads::window_owner_for_space(&home_space)
                            != Some(target_owner_id)
                            || target_term_window.frontend_recovery_slot()
                                != FrontendRecoverySlot::Window(target_owner_id)
                        {
                            return;
                        }
                        target_term_window
                            .activate_workspace_thread(thread_id, &target_window);
                        target_window.focus();
                    },
                )));
                return;
            }
            // No window owns that Space: navigate this one there, exactly
            // as opening a windowless Space's thread does elsewhere.
            if let Some(window) = self.window.clone() {
                self.switch_space_to_thread(home_space, Some(thread_id), &window);
            }
        }
    }

    /// Focus the tab and pane hosting this agent, when it lives in this
    /// window.
    fn reveal_agent_pane(&mut self, pane_id: PaneId) {
        let mux = Mux::get();
        let Some((_domain, window_id, tab_id)) = mux.resolve_pane_id(pane_id) else {
            return;
        };
        if window_id != self.mux_window_id {
            return;
        }
        let tab_idx = {
            let Some(window) = mux.get_window(window_id) else {
                return;
            };
            (0..window.len()).find(|idx| {
                window
                    .get_by_idx(*idx)
                    .map(|tab| tab.tab_id() == tab_id)
                    .unwrap_or(false)
            })
        };
        if let Some(idx) = tab_idx {
            if let Some(mut window) = mux.get_window_mut(window_id) {
                window.save_and_then_set_active(idx);
            }
        }
        if let (Some(tab), Some(pane)) = (mux.get_tab(tab_id), mux.get_pane(pane_id)) {
            tab.set_active_pane(&pane);
        }
    }
}

/// The slice of a list row that is actually inside the panel.
#[derive(Debug, PartialEq)]
struct AgentRowBand {
    /// Quantized top of the whole row, clipped or not. Row content is laid
    /// out from here so it keeps its place inside the card while the row
    /// scrolls under an edge.
    top: usize,
    /// Top of the part of the card that is inside `[clip_top, clip_bottom)`.
    visible_y: usize,
    visible_height: usize,
}

/// Clip one list row to the panel, the way `paint_snippet_card` clips a
/// snippet card: rows scroll under both edges, so a row can be cut by the
/// list origin above and by the panel edge below.
///
/// `row_top` is fractional so scrolling stays smooth, and goes negative once
/// a row has scrolled off the top. Returns `None` when no part of the row is
/// inside the band.
fn agent_row_visible_band(
    row_top: f32,
    row_height: usize,
    clip_top: usize,
    clip_bottom: usize,
) -> Option<AgentRowBand> {
    if row_height == 0 || clip_bottom <= clip_top {
        return None;
    }
    if row_top + row_height as f32 <= clip_top as f32 || row_top >= clip_bottom as f32 {
        return None;
    }
    // `max(0.0)` only bites when the list origin sits within one row height
    // of the window top, which the sidebar layout never does; it is here so
    // the cast of a negative top cannot wrap.
    let top = row_top.floor().max(0.0) as usize;
    let visible_y = top.max(clip_top);
    let visible_bottom = top.saturating_add(row_height).min(clip_bottom);
    let visible_height = visible_bottom.saturating_sub(visible_y);
    if visible_height == 0 {
        return None;
    }
    Some(AgentRowBand {
        top,
        visible_y,
        visible_height,
    })
}

#[cfg(test)]
mod tests {
    use super::{agent_row_visible_band, AgentRowBand};

    const H: usize = 52;
    const TOP: usize = 100;
    const BOTTOM: usize = 400;

    fn band(row_top: f32) -> Option<AgentRowBand> {
        agent_row_visible_band(row_top, H, TOP, BOTTOM)
    }

    #[test]
    fn fully_visible_row_is_unclipped() {
        assert_eq!(
            band(100.0),
            Some(AgentRowBand {
                top: 100,
                visible_y: 100,
                visible_height: 52
            })
        );
    }

    #[test]
    fn row_clipped_by_the_list_origin_keeps_its_content_anchor() {
        // `top` stays at the row's real top so the icons and text inside the
        // card do not slide while the row scrolls under the edge.
        assert_eq!(
            band(90.0),
            Some(AgentRowBand {
                top: 90,
                visible_y: 100,
                visible_height: 42
            })
        );
    }

    #[test]
    fn row_clipped_by_the_panel_edge() {
        assert_eq!(
            band(380.0),
            Some(AgentRowBand {
                top: 380,
                visible_y: 380,
                visible_height: 20
            })
        );
    }

    #[test]
    fn fractional_tops_quantize_down() {
        assert_eq!(
            band(100.6),
            Some(AgentRowBand {
                top: 100,
                visible_y: 100,
                visible_height: 52
            })
        );
    }

    #[test]
    fn rows_entirely_outside_the_band_are_dropped() {
        assert_eq!(band(48.0), None, "bottom edge exactly on clip_top");
        assert_eq!(band(47.0), None);
        assert_eq!(band(400.0), None, "top edge exactly on clip_bottom");
        assert_eq!(band(401.0), None);
    }

    #[test]
    fn a_row_taller_than_the_panel_fills_it() {
        assert_eq!(
            agent_row_visible_band(90.0, 500, TOP, BOTTOM),
            Some(AgentRowBand {
                top: 90,
                visible_y: 100,
                visible_height: 300
            })
        );
    }

    #[test]
    fn degenerate_inputs_paint_nothing() {
        assert_eq!(agent_row_visible_band(100.0, 0, TOP, BOTTOM), None);
        assert_eq!(agent_row_visible_band(100.0, H, BOTTOM, TOP), None);
        assert_eq!(agent_row_visible_band(100.0, H, TOP, TOP), None);
    }

    #[test]
    fn negative_tops_clamp_instead_of_wrapping() {
        assert_eq!(
            agent_row_visible_band(-10.0, H, 0, BOTTOM),
            Some(AgentRowBand {
                top: 0,
                visible_y: 0,
                visible_height: 52
            })
        );
    }
}
