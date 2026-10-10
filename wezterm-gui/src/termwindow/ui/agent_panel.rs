//! The right-sidebar Agents panel: lists panes running detected coding
//! agents with their live state. Painted only while the agent-panel
//! feature toggle is on; all data comes from `crate::agent_status`
//! snapshots — the paint path never triggers detection or filesystem
//! probes.

use crate::agent_status::{self, AgentIcon, AgentPanelAction, ProgramReportState};
use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::ui::right_sidebar::{
    right_sidebar_file_row_metrics, sidebar_row_element_visible, FILE_SCROLL_FADE_HEIGHT,
};
use crate::termwindow::ui::tokens::{SIDEBAR_ICON_GAP, SIDEBAR_INSET, SIDEBAR_ROW_RADIUS};
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

/// An agent that is working, in the same blue the left sidebar uses for a
/// live thread (`SESSION_STATUS_OPEN_COLOR`).
const AGENT_WORKING_COLOR: LinearRgba = LinearRgba::with_components(0.12, 0.48, 1.0, 1.0);
/// An agent waiting on the user: amber, matching the sidebar's other
/// "needs a human" signal. Deliberately not the notification red, which is
/// reserved for something being wrong rather than something being asked.
pub(crate) const AGENT_BLOCKED_COLOR: LinearRgba = LinearRgba::with_components(0.86, 0.45, 0.12, 1.0);
/// A finished result nobody has looked at yet: the green the left sidebar
/// gives a finished thread, the same constant so the two cannot drift.
pub(crate) const AGENT_DONE_COLOR: LinearRgba =
    crate::termwindow::ui::sidebar::SESSION_STATUS_DONE_COLOR;
/// The progress bar under a row's description.
const AGENT_PROGRESS_HEIGHT: usize = 6;
/// Between two agent rows; a hairline sits in the middle of it.
const AGENT_ROW_GAP: usize = 10;
/// How much further in than the panel's inset agent rows start and end.
const AGENT_ROW_EXTRA_INSET: usize = 6;
/// Sub-task lines a row shows before it says how many more there are.
const AGENT_SUBTASKS_SHOWN: usize = 5;

impl TermWindow {
    /// The strip above the list: one row holding the status line and the
    /// view-menu button, plus the gap before the first row. It doubles as the
    /// top scroll mask, so this is also how far overflow may be erased.
    fn agents_toolbar_height(&self, cell_height: usize) -> usize {
        self.agents_toolbar_button_height(cell_height) + self.ui_px(8)
    }

    /// Two lines -- the title, then where it runs -- with the same share of
    /// vertical padding a file row gives its single line. The lines of a
    /// card sit close: they are one agent, and the gaps between cards and
    /// the hairlines are what part them.
    fn agents_row_height(&self, cell_height: usize) -> (usize, usize) {
        let line_gap = (cell_height / 20).max(1);
        let padding = (cell_height * 7 / 10).max(1);
        (cell_height * 2 + line_gap + padding, line_gap)
    }

    /// The view-menu button: a compact icon button in the top-right, the
    /// same shape the Files panel gives its refresh.
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
        has_agents: bool,
    ) -> anyhow::Result<()> {
        let cell_height = ui_metrics.cell_size.height as usize;
        let button_height = self.agents_toolbar_button_height(cell_height);
        let button_size = button_height.min(content_width);
        // The view menu, at the right edge: what to show, how to group and
        // order it, which parts of a report to show, and the rules reload.
        let view_x = content_x + content_width.saturating_sub(button_size);
        self.paint_files_preview_header_icon_button(
            layers,
            chrome,
            foreground,
            muted_fg,
            view_x,
            content_top,
            button_size,
            SvgIcon::SlidersHorizontal,
            UIItemType::RightSidebarAgent(AgentPanelAction::ViewOptions),
        )?;

        // The line to the left of that button, only when there is something
        // to say: a reload acknowledgement while it lives, because it is the
        // answer to the click that just happened, or why the list is empty.
        // A running count would only repeat the group headings, cut short.
        let detection_on = agent_status::enabled();
        let line = match status {
            Some(status) => status.to_string(),
            None if !detection_on => crate::i18n::tr("right-agents-detection-off"),
            None if !has_agents => crate::i18n::tr("right-agents-none"),
            None => String::new(),
        };
        if status.is_some() {
            // The acknowledgement expires by TTL, but expiry alone paints
            // nothing; keep a lazy repaint scheduled while it is visible.
            self.update_next_frame_time(Some(
                std::time::Instant::now() + std::time::Duration::from_millis(500),
            ));
        }
        let text_x = content_x + self.ui_px(SIDEBAR_INSET);
        let text_y = content_top + (button_height.saturating_sub(cell_height)) / 2;
        let text_width = view_x
            .saturating_sub(self.ui_px(SIDEBAR_ICON_GAP))
            .saturating_sub(text_x);
        self.paint_agent_text(
            layers, ui_font, ui_metrics, &line, text_x, text_y, text_width, muted_fg,
        )?;
        // Only the "detection is off" line is actionable: it names the
        // switch that turns the panel back on, so it must lead there.
        if status.is_none() && !detection_on {
            self.ui_items.push(UIItem {
                x: text_x,
                y: content_top,
                width: text_width,
                height: button_height,
                item_type: UIItemType::RightSidebarAgent(AgentPanelAction::OpenSettings),
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_agents_sidebar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        content_x: usize,
        content_top: usize,
        content_width: usize,
        content_bottom: usize,
    ) -> anyhow::Result<()> {
        // The sidebar's own face is medium weight -- bold off macOS -- and
        // with every line of a card in it, a semibold title barely stood
        // out. The panel sets its lines in the regular weight instead, so
        // the titles and headings, semibold, read as such.
        let settings = crate::native_settings::load_shared();
        let font_size = crate::native_settings::right_sidebar_font_size(&settings);
        let body_font = self
            .fonts
            .title_font_with_size_and_weight(font_size, 400)
            .context("agents panel body font")?;
        let ui_font = &body_font;
        let ui_metrics = RenderMetrics::with_font_metrics(&body_font.metrics());
        let cell_height = ui_metrics.cell_size.height as usize;
        let (row_height, line_gap) = self.agents_row_height(cell_height);
        // Read once: the status expires on a TTL, and asking twice could size
        // the toolbar for one layout and paint another.
        let status = agent_status::panel_status();
        let toolbar_height = self.agents_toolbar_height(cell_height);
        let list_top = content_top + toolbar_height;
        let inset = self.ui_px(SIDEBAR_INSET);

        let mut agents = agent_status::list_agent_panes();
        // An open list lasts as long as its sub-tasks: the next batch starts
        // folded, and an agent that has gone leaves nothing behind.
        self.right_sidebar_agents_fitted
            .retain(|pane_id, _| agents.iter().any(|agent| agent.pane_id == *pane_id));
        self.right_sidebar_agents_open_subtasks.retain(|pane_id| {
            agents.iter().any(|agent| {
                agent.pane_id == *pane_id
                    && agent
                        .report
                        .as_ref()
                        .is_some_and(|report| !report.children.is_empty())
            })
        });
        let has_agents = !agents.is_empty();
        if agents.is_empty() {
            // Nothing but the toolbar: an empty-state card here is a large
            // bordered box explaining that a list is empty, which the
            // toolbar line already says in one row. The panel reads as
            // quiet rather than broken.
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
                has_agents,
            );
        }

        // The view menu's choices: what is hidden, then how the rest is
        // grouped and ordered.
        // Borrowed from the shared settings, not copied: this runs per frame.
        let view = &settings.chrome.agents_panel_view;
        agents.retain(|agent| {
            !view.hides_status(agent.work_status().settings_key())
                && !view.hides_machine(&agent.machine)
        });
        agent_status::sort_for_display(&mut agents);
        order_for_view(&mut agents, view);
        let entries = panel_entries(&agents, view.group_by());

        // Borrow the file tree's row chrome wholesale so the two lists read as
        // one control: same height, same icon size, same gaps, all derived from
        // the font instead of from physical-pixel constants. Rows sit flush
        // against each other -- no gap, no card, no border.
        let metrics = right_sidebar_file_row_metrics(ui_metrics);
        // The panel's bottom padding, which doubles as the mask that keeps
        // rows out of it. Whatever overshoots it runs past the panel, where
        // the window edge cuts it: the panel runs the full window height.
        let viewport_bottom = content_bottom.saturating_sub(inset * 2);
        let overflow_bottom = usize::MAX;
        // Rows run to the window edge and are cut there; their hover chrome
        // must reach as far, or glyphs outrun their tint.
        let row_clip_bottom = content_bottom;
        // An element is painted while it straddles an edge, because something
        // cuts it: the header mask above, the window edge below. That is the
        // whole difference between this and withholding it until it fits,
        // which is what made the list step.
        let visible = |elem_y: usize, elem_height: usize| {
            sidebar_row_element_visible(elem_y, elem_height, content_top, list_top, overflow_bottom)
        };
        let visible_height = viewport_bottom.saturating_sub(list_top);
        // Each row starts a gap below the last, and the last has no trailing
        // gap, so it is not part of the scrollable height either. The gap is
        // tighter than the snippet cards' now that a hairline parts the
        // rows. A row is two lines tall, and grows below them by what its
        // program reported, as far as the user chose to see it.
        let row_gap = self.ui_px(AGENT_ROW_GAP);
        let show = crate::native_settings::agent_status_display();
        // A card is its title, semibold and bright, over lines in one grey:
        // weight sets the title apart, so the lines under it need not
        // compete in brightness. Errors stay red; a working description
        // shimmers.
        // Between rows of a group: plain enough to stay out of the way, but
        // a shade firmer than the separator token, which vanished here.
        let divider = muted_fg.mul_alpha(0.22);
        // A working description shimmers from the quieter grey up to white
        // (black on a light theme): from the brighter grey the light that
        // passed was too close to the text to see.
        let shimmer_peak = if chrome.is_dark() {
            LinearRgba::with_components(1.0, 1.0, 1.0, foreground.3)
        } else {
            LinearRgba::with_components(0.0, 0.0, 0.0, foreground.3)
        };
        // Its colours are blended once a frame, not once a glyph: steps this
        // fine are closer than the eye tells apart across a band this narrow.
        let shimmer_ramp: [LinearRgba; SHIMMER_RAMP_STEPS] = std::array::from_fn(|step| {
            crate::ui::color::mix(
                muted_fg,
                shimmer_peak,
                step as f32 / (SHIMMER_RAMP_STEPS - 1) as f32,
            )
        });
        let bar_height = self.ui_px(AGENT_PROGRESS_HEIGHT);
        // Every row puts its text between the same two edges; a row with no
        // state icon reaches further right.
        // Rows keep a little more room from the panel's edges than its
        // other lines: the brand marks sat against the edge.
        let row_inset = inset + self.ui_px(AGENT_ROW_EXTRA_INSET);
        let icon_x = content_x + row_inset;
        // Both icons a line of text tall: they sit on the title line, and at
        // the file tree's size they stood taller than the line they marked.
        let icon_size = metrics.icon_size.min(cell_height);
        let text_x = icon_x + icon_size + metrics.icon_gap;
        let state_size = icon_size;
        let state_x = content_x + content_width - row_inset - state_size;
        let text_right_for = |has_state_icon: bool| {
            if has_state_icon {
                state_x.saturating_sub(metrics.icon_gap)
            } else {
                content_x + content_width - row_inset
            }
        };
        // One clock reading for the whole panel, so its spinners and
        // shimmers step together.
        let frames = crate::termwindow::ui::status_icon::spinner_frames();
        let layouts: Vec<AgentRowLayout> = agents
            .iter()
            .map(|agent| {
                AgentRowLayout::of(
                    agent,
                    show,
                    self.right_sidebar_agents_open_subtasks
                        .contains(&agent.pane_id),
                    row_height,
                    cell_height + line_gap,
                    bar_height + line_gap,
                )
            })
            .collect();
        // Card titles and group headings: the rows' size, semibold. Shaped
        // once and cached like every sidebar line, so the second weight
        // costs nothing per frame. Headings take the grey, so a heading
        // does not compete with the titles under it.
        let header_font = self
            .fonts
            .title_font_with_size_and_weight(font_size, 600)
            .context("agents group heading font")?;
        let header_metrics = RenderMetrics::with_font_metrics(&header_font.metrics());
        let header_height = header_metrics.cell_size.height as usize;
        // Where each entry starts: a row a gap below whatever came before it,
        // a group's first row a little closer under its heading.
        let header_gap = self.ui_px(4);
        let mut tops = Vec::with_capacity(entries.len());
        let mut total_height = 0usize;
        for (index, entry) in entries.iter().enumerate() {
            if index > 0 {
                total_height += match entries[index - 1] {
                    PanelEntry::Header { .. } => header_gap,
                    PanelEntry::Agent(_) => row_gap,
                };
            }
            tops.push(total_height);
            total_height += match *entry {
                PanelEntry::Header { .. } => header_height,
                PanelEntry::Agent(row) => layouts[row].height,
            };
        }
        let max_scroll = total_height.saturating_sub(visible_height) as f32;
        self.right_sidebar_agents_scroll = self.right_sidebar_agents_scroll.clamp(0.0, max_scroll);
        let scroll = self.right_sidebar_agents_scroll;

        if entries.is_empty() {
            // Agents there are, but the view menu hides every one: say so
            // where the list would be, or the panel reads as broken.
            let line = crate::i18n::tr("right-agents-all-hidden");
            self.paint_agent_text(
                layers,
                ui_font,
                ui_metrics,
                &line,
                content_x + inset,
                list_top,
                content_width.saturating_sub(inset * 2),
                muted_fg,
            )?;
        }
        for (index, entry) in entries.iter().enumerate() {
            let row_top = list_top as f32 + tops[index] as f32 - scroll;
            if row_top >= viewport_bottom as f32 {
                break;
            }
            let row = match entry {
                PanelEntry::Header { label, count } => {
                    let y = row_top.floor().max(0.0) as usize;
                    if visible(y, header_height) {
                        // The count keeps its room; a long thread name is
                        // what gives way.
                        let count = count.to_string();
                        let gap = self.ui_px(SIDEBAR_ICON_GAP);
                        let count_width = self
                            .cached_ui_text_advance(&header_font, &header_metrics, &count)?
                            .ceil() as usize;
                        let label_x = icon_x;
                        // Between the rows' edges: the count ends where
                        // their state icons do.
                        let label_room = (content_x + content_width)
                            .saturating_sub(row_inset)
                            .saturating_sub(label_x)
                            .saturating_sub(count_width + gap);
                        let label_width = (self
                            .cached_ui_text_advance(&header_font, &header_metrics, label)?
                            .ceil() as usize)
                            .min(label_room);
                        self.paint_agent_text(
                            layers,
                            &header_font,
                            header_metrics,
                            label,
                            label_x,
                            y,
                            label_room,
                            muted_fg,
                        )?;
                        self.paint_agent_text(
                            layers,
                            &header_font,
                            header_metrics,
                            &count,
                            label_x + label_width + gap,
                            y,
                            count_width,
                            muted_fg,
                        )?;
                    }
                    continue;
                }
                PanelEntry::Agent(row) => *row,
            };
            let (agent, layout) = (&agents[row], &layouts[row]);
            // A hairline in the gap above a row that follows another in its
            // group, in the separator colour: it parts the agents without
            // boxing them in. A heading parts the groups.
            if index > 0 && matches!(entries[index - 1], PanelEntry::Agent(_)) {
                let y = (row_top - row_gap as f32 / 2.0).floor();
                if y >= list_top as f32 && y < viewport_bottom as f32 {
                    self.fill_rounded_rectangle(
                        layers,
                        1,
                        euclid::rect(
                            (content_x + inset) as f32,
                            y,
                            content_width.saturating_sub(inset * 2) as f32,
                            1.0,
                        ),
                        divider,
                        0.0,
                    )
                    .context("agent row divider")?;
                }
            }
            if row_top + layout.height as f32 <= list_top as f32 {
                continue;
            }
            let Some(band) =
                agent_row_visible_band(row_top, layout.height, list_top, row_clip_bottom)
            else {
                continue;
            };
            let top = band.top;
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
                    self.ui_f32(SIDEBAR_ROW_RADIUS) + 10.0,
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

            // Both icons sit on the title line, however far the row grows
            // below it.
            let title_y = top + (row_height.saturating_sub(cell_height * 2 + line_gap)) / 2;
            let title_mid = title_y + cell_height / 2;
            let icon_y = title_mid.saturating_sub(icon_size / 2);
            if visible(icon_y, icon_size) {
                match agent_status::brand_icon(
                    &agent.agent_id,
                    crate::native_settings::effective_appearance(),
                ) {
                    Some(AgentIcon::Color(brand)) => self.paint_sidebar_brand_icon(
                        layers,
                        brand,
                        icon_x,
                        icon_y,
                        icon_size,
                    )?,
                    Some(AgentIcon::Mono(icon)) => self.paint_sidebar_icon(
                        layers,
                        icon,
                        icon_x,
                        icon_y,
                        icon_size,
                        muted_fg,
                    )?,
                    None => self.paint_sidebar_icon(
                        layers,
                        SvgIcon::Bot,
                        icon_x,
                        icon_y,
                        icon_size,
                        muted_fg,
                    )?,
                }
            }

            let shown = agent.shown_state();
            let state_y = title_mid.saturating_sub(state_size / 2);
            let state_icon = agent_state_icon(shown, chrome, muted_fg);
            let has_state_icon = state_icon.is_some();
            if let Some((icon, color, spinning)) =
                state_icon.filter(|_| visible(state_y, state_size))
            {
                if spinning {
                    self.paint_spinning_ui_icon_at(
                        layers, 2, icon, state_x, state_y, state_size, color, frames,
                    )?;
                } else {
                    self.paint_sidebar_icon(layers, icon, state_x, state_y, state_size, color)?;
                }
            }

            let text_right = text_right_for(has_state_icon);
            let text_width = text_right.saturating_sub(text_x);
            let name = agent_status::display_name(&agent.agent_id);
            let title_line = if agent.title.is_empty() {
                name
            } else {
                format!("{name} · {}", agent.title)
            };
            if visible(title_y, cell_height) {
                self.paint_agent_text(
                    layers,
                    &header_font,
                    header_metrics,
                    &title_line,
                    text_x,
                    title_y,
                    text_width,
                    foreground,
                )?;
            }

            // Under the title: what it is doing, then where. The state icon
            // already says the state, so the words only stand in for it
            // when there is no icon, or nothing else to put on the line --
            // and for Idle, whose check differs from Done's only in colour.
            let mut line_y = title_y + cell_height + line_gap;
            if let Some(report) = agent.report.as_ref().filter(|_| layout.message) {
                if visible(line_y, cell_height) {
                    let mut x = text_x;
                    if let Some(kind) = report.kind.filter(|_| show.reason) {
                        let label = crate::i18n::tr(agent_status::blocked_kind_key(kind));
                        x = self.paint_agent_kind_pill(
                            layers, ui_font, ui_metrics, &label, x, line_y, text_right,
                        )?;
                    }
                    if let Some(msg) = report.msg.as_deref().filter(|_| show.description) {
                        let width = text_right.saturating_sub(x);
                        let fitted =
                            self.fitted_description(agent.pane_id, ui_font, &ui_metrics, msg, width)?;
                        let msg = fitted.as_deref().unwrap_or(msg);
                        match shown {
                            agent_status::ShownState::Working => {
                                let advance =
                                    self.cached_ui_text_advance(ui_font, &ui_metrics, msg)?;
                                let travel = advance.min(width as f32) + (x - text_x) as f32;
                                let shimmer = ShimmerBand::at(frames, travel, cell_height as f32);
                                self.paint_agent_shimmer_text(
                                    layers,
                                    ui_font,
                                    ui_metrics,
                                    msg,
                                    x,
                                    line_y,
                                    width,
                                    text_x,
                                    shimmer,
                                    &shimmer_ramp,
                                )?
                            }
                            agent_status::ShownState::Error => self.paint_agent_text(
                                layers,
                                ui_font,
                                ui_metrics,
                                msg,
                                x,
                                line_y,
                                width,
                                chrome.danger,
                            )?,
                            _ => self.paint_agent_text(
                                layers, ui_font, ui_metrics, msg, x, line_y, width, muted_fg,
                            )?,
                        }
                    }
                }
                line_y += cell_height + line_gap;
            }
            // The thread's human name, resolved once per snapshot -- never the
            // internal workspace id, and never a per-frame store walk.
            let place_line = if has_state_icon
                && shown != agent_status::ShownState::Idle
                && !agent.place.is_empty()
            {
                agent.place.clone()
            } else {
                let state_label = crate::i18n::tr(agent_state_key(shown));
                if agent.place.is_empty() {
                    state_label
                } else {
                    format!("{state_label} · {}", agent.place)
                }
            };
            if visible(line_y, cell_height) {
                self.paint_agent_text(
                    layers,
                    ui_font,
                    ui_metrics,
                    &place_line,
                    text_x,
                    line_y,
                    text_width,
                    muted_fg,
                )?;
            }
            line_y += cell_height + line_gap;

            let Some(report) = agent.report.as_ref() else {
                continue;
            };
            if let Some(progress) = layout.progress {
                if visible(line_y, bar_height) {
                    let radius = bar_height as f32 / 2.0;
                    self.fill_rounded_rectangle(
                        layers,
                        1,
                        euclid::rect(
                            text_x as f32,
                            line_y as f32,
                            text_width as f32,
                            bar_height as f32,
                        ),
                        chrome.separator,
                        radius,
                    )
                    .context("agent progress track")?;
                    let filled = (text_width as f32 * f32::from(progress) / 100.0).round();
                    if filled >= bar_height as f32 {
                        self.fill_rounded_rectangle(
                            layers,
                            1,
                            euclid::rect(text_x as f32, line_y as f32, filled, bar_height as f32),
                            chrome.accent,
                            radius,
                        )
                        .context("agent progress")?;
                    }
                }
                line_y += bar_height + line_gap;
            }
            let sub_icon = (cell_height * 4 / 5).max(1);
            let sub_text_gap = sub_icon + self.ui_px(SIDEBAR_ICON_GAP) / 2;
            // An open list's guide drops from just under the chevron.
            let guide_top = line_y + (cell_height.saturating_sub(sub_icon)) / 2 + sub_icon;
            if layout.subtask_count > 0 {
                // The fold line is its own target over the row's: a click on
                // it opens or folds the list rather than revealing the pane.
                let hit_top = line_y.max(band.visible_y);
                let hit_bottom = (line_y + cell_height + line_gap)
                    .min(band.visible_y + band.visible_height);
                let mut fold_hovered = false;
                if hit_bottom > hit_top {
                    fold_hovered = self.is_pointer_over_ui_rect(
                        content_x,
                        hit_top,
                        content_width,
                        hit_bottom - hit_top,
                    );
                    self.ui_items.push(UIItem {
                        x: content_x,
                        y: hit_top,
                        width: content_width,
                        height: hit_bottom - hit_top,
                        item_type: UIItemType::RightSidebarAgent(
                            AgentPanelAction::ToggleSubtasks(agent.pane_id),
                        ),
                    });
                }
                if visible(line_y, cell_height) {
                    let chevron = if layout.subtasks > 0 {
                        SvgIcon::ChevronDown
                    } else {
                        SvgIcon::ChevronRight
                    };
                    let icon_y = line_y + (cell_height.saturating_sub(sub_icon)) / 2;
                    self.paint_sidebar_icon(layers, chevron, text_x, icon_y, sub_icon, muted_fg)?;
                    let mut args = fluent_bundle::FluentArgs::new();
                    args.set("count", layout.subtask_count);
                    let mut line = crate::i18n::tr_args("right-agents-subtasks", &args);
                    let working = report
                        .children
                        .iter()
                        .filter(|child| matches!(child.state, ProgramReportState::Working))
                        .count();
                    // Unless every sub-task has its dot in view, this line is
                    // all there is to say how many run: folded, or with some
                    // cut off below the list.
                    if working > 0 && layout.subtasks < layout.subtask_count {
                        let mut args = fluent_bundle::FluentArgs::new();
                        args.set("count", working);
                        let working = crate::i18n::tr_args("right-agents-count-working", &args);
                        line = format!("{line} · {working}");
                    }
                    let line_x = text_x + sub_text_gap;
                    self.paint_agent_text(
                        layers,
                        ui_font,
                        ui_metrics,
                        &line,
                        line_x,
                        line_y,
                        text_right.saturating_sub(line_x),
                        if fold_hovered {
                            foreground
                        } else {
                            muted_fg
                        },
                    )?;
                }
                line_y += cell_height + line_gap;
            }
            // An open list sits under the fold line's words, a level in, and
            // hangs off a guide dropped from the chevron.
            let list_x = text_x + sub_text_gap;
            for child in report.children.iter().take(layout.subtasks) {
                if visible(line_y, cell_height) {
                    let child_state = match child.state {
                        ProgramReportState::Working => agent_status::ShownState::Working,
                        ProgramReportState::Blocked => agent_status::ShownState::Blocked,
                        ProgramReportState::Error => agent_status::ShownState::Error,
                        ProgramReportState::Done => agent_status::ShownState::Done,
                        ProgramReportState::Idle => agent_status::ShownState::Idle,
                    };
                    // A dot in the state's colour, never a spinner: only the
                    // row's own spinner and its description move.
                    if let Some((_, color, _)) = agent_state_icon(child_state, chrome, muted_fg) {
                        let dot = (cell_height * 2 / 5).max(self.ui_px(4));
                        self.fill_rounded_rectangle(
                            layers,
                            1,
                            euclid::rect(
                                (list_x + sub_icon.saturating_sub(dot) / 2) as f32,
                                (line_y + cell_height.saturating_sub(dot) / 2) as f32,
                                dot as f32,
                                dot as f32,
                            ),
                            color,
                            dot as f32 / 2.0,
                        )
                        .context("agent sub-task dot")?;
                    }
                    let name = child
                        .title
                        .as_deref()
                        .or(child.msg.as_deref())
                        .unwrap_or(&child.id);
                    let line = match child.kind.filter(|_| show.reason) {
                        Some(kind) => format!(
                            "{name} · {}",
                            crate::i18n::tr(agent_status::blocked_kind_key(kind))
                        ),
                        None => name.to_string(),
                    };
                    let line_x = list_x + sub_text_gap;
                    self.paint_agent_text(
                        layers,
                        ui_font,
                        ui_metrics,
                        &line,
                        line_x,
                        line_y,
                        text_right.saturating_sub(line_x),
                        muted_fg,
                    )?;
                }
                line_y += cell_height + line_gap;
            }
            if layout.subtasks > 0 {
                // Down to the last dot's line; the "more" line has no dot.
                let top = guide_top.max(band.visible_y);
                let bottom = line_y
                    .saturating_sub(line_gap)
                    .min(band.visible_y + band.visible_height);
                if bottom > top {
                    let guide = self.ui_px(1).max(1);
                    self.fill_rounded_rectangle(
                        layers,
                        1,
                        euclid::rect(
                            (text_x + sub_icon / 2) as f32,
                            top as f32,
                            guide as f32,
                            (bottom - top) as f32,
                        ),
                        chrome.separator,
                        0.0,
                    )
                    .context("agent sub-task guide")?;
                }
            }
            if layout.more_subtasks > 0 && visible(line_y, cell_height) {
                let mut args = fluent_bundle::FluentArgs::new();
                args.set("count", layout.more_subtasks);
                let more = crate::i18n::tr_args("right-agents-more-subtasks", &args);
                self.paint_agent_text(
                    layers,
                    ui_font,
                    ui_metrics,
                    &more,
                    list_x,
                    line_y,
                    text_right.saturating_sub(list_x),
                    muted_fg,
                )?;
            }
        }

        let scrolled = max_scroll > 0.0 && scroll > 0.0;
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
            has_agents,
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

    /// One line of the panel in one colour. See `paint_agent_text_with`.
    #[allow(clippy::too_many_arguments)]
    fn paint_agent_text(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        self.paint_agent_text_with(layers, font, metrics, text, x, y, width, |_| color)
    }

    /// One line of the panel, whole when it fits; when it does not, faded
    /// out towards its right edge rather than cut short with an ellipsis.
    /// `color_at` is asked for each glyph by where its middle falls, from
    /// `x`. The painter visits glyphs in order and asks before drawing each,
    /// so the pen here keeps pace with its; nothing is allocated, and a
    /// glyph the fade has taken to nothing is where drawing stops.
    #[allow(clippy::too_many_arguments)]
    fn paint_agent_text_with(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        mut color_at: impl FnMut(f32) -> LinearRgba,
    ) -> anyhow::Result<()> {
        if text.is_empty() || width == 0 {
            return Ok(());
        }
        let (shaped, _) = self.cached_ui_shape(font, &metrics, text)?;
        let advance: f32 = shaped
            .iter()
            .map(|info| info.glyph.x_advance.get() as f32)
            .sum();
        let width = width as f32;
        let fade = if advance > width {
            self.ui_f32(AGENT_TEXT_FADE).min(width)
        } else {
            0.0
        };
        let mut pen = 0.0f32;
        self.paint_cached_ui_shape_clipped(
            layers,
            &metrics,
            &shaped,
            x as f32,
            y as f32,
            x as f32,
            x as f32 + width,
            |info| {
                let glyph_advance = info.glyph.x_advance.get() as f32;
                let middle = pen + glyph_advance / 2.0;
                pen += glyph_advance;
                let color = color_at(middle);
                if fade > 0.0 {
                    // Eased, so the glyphs at the edge are all but gone and
                    // the one too long to fit is not missed.
                    let t = ((width - middle) / fade).clamp(0.0, 1.0);
                    color.mul_alpha(t * t)
                } else {
                    color
                }
            },
        )?;
        Ok(())
    }

    /// One line of a working program's description, lit where `shimmer`
    /// crosses it; `left` is the edge the band is measured from, and `ramp`
    /// runs from the text's colour to the light. The band steps on the
    /// spinner's clock and asks for that same frame, so a row that shimmers
    /// repaints no more often than its spinner already makes it. Each
    /// glyph's colour is picked from the ramp as it is drawn: nothing is
    /// allocated or blended here.
    #[allow(clippy::too_many_arguments)]
    fn paint_agent_shimmer_text(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        left: usize,
        shimmer: ShimmerBand,
        ramp: &[LinearRgba; SHIMMER_RAMP_STEPS],
    ) -> anyhow::Result<()> {
        let offset = x as f32 - left as f32;
        self.paint_agent_text_with(layers, font, metrics, text, x, y, width, |middle| {
            let lit = shimmer.strength(offset + middle);
            ramp[(lit * (SHIMMER_RAMP_STEPS - 1) as f32).round() as usize]
        })?;
        self.request_spinner_frame();
        Ok(())
    }

    /// An agent's description as it best fits `width`: `None` to draw it as
    /// it is -- it fits, or it has no path to cut -- else with the paths in it
    /// cut back to their last few parts, as many as fit, down to the file
    /// name, since the end of a path is what tells you most. What does not
    /// fit even then is left to fade. Worked out once per description,
    /// width and font, and kept for the agent: cutting means trying every
    /// depth, which a spinner repainting every frame would otherwise redo.
    fn fitted_description(
        &mut self,
        pane_id: PaneId,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        msg: &str,
        width: usize,
    ) -> anyhow::Result<Option<Rc<str>>> {
        if let Some(memo) = self.right_sidebar_agents_fitted.get(&pane_id) {
            if memo.msg == msg && memo.width == width && memo.font == font.id() {
                return Ok(memo.fitted.clone());
            }
        }
        let room = width as f32;
        let mut fitted = None;
        if self.cached_ui_text_advance(font, metrics, msg)? > room {
            let deepest = msg.split_whitespace().map(path_parts).max().unwrap_or(0);
            for keep in (1..deepest).rev() {
                let Some(shorter) = shorten_paths(msg, keep) else {
                    continue;
                };
                let fits = self.cached_ui_text_advance(font, metrics, &shorter)? <= room;
                fitted = Some(Rc::<str>::from(shorter));
                if fits {
                    break;
                }
            }
        }
        self.right_sidebar_agents_fitted.insert(
            pane_id,
            FittedDescription {
                msg: msg.to_string(),
                width,
                font: font.id(),
                fitted: fitted.clone(),
            },
        );
        Ok(fitted)
    }

    /// The pill naming what a blocked program waits for. Returns where the
    /// text after it starts.
    #[allow(clippy::too_many_arguments)]
    fn paint_agent_kind_pill(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        label: &str,
        x: usize,
        y: usize,
        right: usize,
    ) -> anyhow::Result<usize> {
        let cell_height = ui_metrics.cell_size.height as usize;
        let pad = self.ui_px(10);
        let width = (self.sidebar_text_width(ui_font, label)?.ceil() as usize + pad * 2)
            .min(right.saturating_sub(x));
        self.fill_rounded_rectangle(
            layers,
            1,
            euclid::rect(x as f32, y as f32, width as f32, cell_height as f32),
            AGENT_BLOCKED_COLOR.mul_alpha(0.18),
            cell_height as f32 / 2.0,
        )
        .context("agent waiting reason")?;
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            label,
            x + pad,
            y,
            width.saturating_sub(pad * 2),
            AGENT_BLOCKED_COLOR,
        )?;
        Ok(x + width + self.ui_px(SIDEBAR_ICON_GAP))
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
            AgentPanelAction::OpenSettings => {
                crate::settings_window::show_from(self.mux_window_id, &self.active_space_id);
            }
            AgentPanelAction::Reveal(pane_id) => {
                self.reveal_agent_pane(pane_id);
            }
            AgentPanelAction::RevealElsewhere(pane_id) => {
                self.reveal_agent_pane_elsewhere(pane_id, context);
            }
            AgentPanelAction::ToggleSubtasks(pane_id) => {
                if !self.right_sidebar_agents_open_subtasks.remove(&pane_id) {
                    self.right_sidebar_agents_open_subtasks.insert(pane_id);
                }
            }
            AgentPanelAction::ViewOptions => {
                let coords = window::Point::new(
                    item.x as isize,
                    item.y.saturating_add(item.height) as isize,
                );
                let items = self.agents_view_options_menu_items();
                self.show_term_context_menu(context, coords, items);
            }
        }
        context.invalidate();
    }

    /// Re-read the agent detection rules. Reading manifest files stays off
    /// the GUI thread; the toolbar line reports how it went.
    pub(crate) fn reload_agent_rules(&mut self) {
        std::thread::spawn(|| {
            let rejected = mux::agent_status::reload_rules();
            promise::spawn::spawn_into_main_thread(async move {
                let ack = if rejected == 0 {
                    crate::i18n::tr("right-agents-rules-reloaded")
                } else {
                    let mut args = fluent_bundle::FluentArgs::new();
                    args.set("count", rejected);
                    crate::i18n::tr_args("right-agents-rules-reload-errors", &args)
                };
                agent_status::set_panel_status(ack);
                if let Some(front_end) = crate::frontend::try_front_end() {
                    front_end.invalidate_all_windows();
                }
            })
            .detach();
        });
    }

    /// The view menu: what to show (by state, and by machine once there is
    /// more than one), how to group and order it, and which parts of a
    /// program's report to show -- the same four switches as Settings ›
    /// Agents, ticked and without icons -- and reloading the detection rules.
    fn agents_view_options_menu_items(&mut self) -> Vec<window::ContextMenuItem> {
        use crate::native_settings::{AgentStatusPart, AgentsGroupBy, AgentsSortBy};
        use crate::termwindow::ContextMenuApplicationAction as Action;
        use window::{ContextMenuIcon, ContextMenuItem};

        self.begin_context_menu_application_actions();
        let view = crate::native_settings::agents_panel_view();
        let display = crate::native_settings::agent_status_display();
        let tr = crate::i18n::tr;

        let status_items = self.status_filter_menu_items(&view.hidden_statuses, |status| {
            Action::ToggleAgentsStatusFilter(status)
        });
        let mut items = vec![ContextMenuItem::submenu_with_icon(
            tr("menu-show"),
            ContextMenuIcon::Filter,
            status_items,
        )];

        // The machines agents are on now; this device first. Named here, on
        // the click, because naming one reads the host list.
        let mut machines: Vec<(String, String)> = Vec::new();
        for agent in agent_status::list_agent_panes() {
            if machines.iter().any(|(key, _)| *key == agent.machine) {
                continue;
            }
            let label = crate::workspace_threads::machine_label_for_workspace(&agent.workspace)
                .filter(|_| !agent.machine.is_empty())
                .unwrap_or_else(|| tr("menu-machine-local"));
            machines.push((agent.machine, label));
        }
        machines.sort_by(|a, b| {
            (!a.0.is_empty())
                .cmp(&!b.0.is_empty())
                .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
        });
        // Offered once there is a choice to make -- or a machine hidden
        // earlier, which would otherwise have no way back.
        if machines.len() > 1 || machines.iter().any(|(key, _)| view.hides_machine(key)) {
            let machine_items = machines
                .into_iter()
                .map(|(key, label)| {
                    let shown = !view.hides_machine(&key);
                    self.context_menu_application_item(label, Action::ToggleAgentsMachineFilter(key))
                        .checked(shown)
                })
                .collect();
            items.push(ContextMenuItem::submenu_with_icon(
                tr("menu-machine"),
                ContextMenuIcon::Server,
                machine_items,
            ));
        }

        items.push(ContextMenuItem::Separator);
        let group_items = AgentsGroupBy::ALL
            .iter()
            .copied()
            .map(|group_by| {
                self.context_menu_application_item(
                    tr(group_by.label_key()),
                    Action::SetAgentsGroupBy(group_by),
                )
                .checked(view.group_by() == group_by)
            })
            .collect();
        items.push(ContextMenuItem::submenu_with_icon(
            tr("menu-group-by"),
            ContextMenuIcon::Group,
            group_items,
        ));
        let sort_items = AgentsSortBy::ALL
            .iter()
            .copied()
            .map(|sort_by| {
                self.context_menu_application_item(
                    tr(sort_by.label_key()),
                    Action::SetAgentsSortBy(sort_by),
                )
                .checked(view.sort_by() == sort_by)
            })
            .collect();
        items.push(ContextMenuItem::submenu_with_icon(
            tr("menu-sort-by"),
            ContextMenuIcon::Sort,
            sort_items,
        ));
        items.push(ContextMenuItem::Separator);
        for part in AgentStatusPart::ALL {
            items.push(
                self.context_menu_application_item(
                    tr(part.label_key()),
                    Action::ToggleAgentStatusPart(part),
                )
                .checked(part.shown(&display)),
            );
        }
        items.push(ContextMenuItem::Separator);
        items.push(self.context_menu_application_item_with_icon(
            tr("menu-reload-agent-rules"),
            ContextMenuIcon::Refresh,
            Action::ReloadAgentRules,
            true,
        ));
        items
    }

    /// Show or hide the agents in one state. Hiding the last state shown
    /// is refused: an empty panel would look broken, not filtered.
    pub(crate) fn toggle_agents_status_filter(
        &mut self,
        status: crate::workspace_threads::WorkspaceThreadWorkStatus,
    ) {
        let mut view = crate::native_settings::agents_panel_view();
        let hidden = std::mem::take(&mut view.hidden_statuses);
        let Some(hidden) = crate::workspace_threads::toggle_hidden_status(hidden, status) else {
            return;
        };
        view.hidden_statuses = hidden;
        self.save_agents_panel_view(view);
    }

    /// Show or hide the agents on one machine. Hiding every machine there
    /// is is refused, for the same reason as hiding every state.
    pub(crate) fn toggle_agents_machine_filter(&mut self, key: String) {
        let mut view = crate::native_settings::agents_panel_view();
        if let Some(index) = view.hidden_machines.iter().position(|hidden| *hidden == key) {
            view.hidden_machines.remove(index);
        } else {
            view.hidden_machines.push(key);
            let mut machines: Vec<String> = agent_status::list_agent_panes()
                .into_iter()
                .map(|agent| agent.machine)
                .collect();
            machines.sort();
            machines.dedup();
            if machines.iter().all(|machine| view.hides_machine(machine)) {
                return;
            }
        }
        self.save_agents_panel_view(view);
    }

    pub(crate) fn set_agents_group_by(&mut self, group_by: crate::native_settings::AgentsGroupBy) {
        let mut view = crate::native_settings::agents_panel_view();
        view.group_by = group_by.key().to_string();
        self.save_agents_panel_view(view);
    }

    pub(crate) fn set_agents_sort_by(&mut self, sort_by: crate::native_settings::AgentsSortBy) {
        let mut view = crate::native_settings::agents_panel_view();
        view.sort_by = sort_by.key().to_string();
        self.save_agents_panel_view(view);
    }

    pub(crate) fn toggle_agent_status_part(
        &mut self,
        part: crate::native_settings::AgentStatusPart,
    ) {
        if let Err(err) = crate::native_settings::toggle_agent_status_part(part) {
            log::warn!("failed to save the agent status display: {err:#}");
        }
        // The cards, tabs and sidebars that show the part are in every
        // window.
        if let Some(front_end) = crate::frontend::try_front_end() {
            front_end.invalidate_all_windows();
        }
    }

    fn save_agents_panel_view(&mut self, view: crate::native_settings::NativeAgentsPanelView) {
        if let Err(err) = crate::native_settings::save_agents_panel_view(view) {
            log::warn!("failed to save the Agents panel view: {err:#}");
        }
        // Every window's panel reads the view as it is now.
        if let Some(front_end) = crate::frontend::try_front_end() {
            front_end.invalidate_all_windows();
        }
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
        if let Some(thread_id) = crate::workspace_threads::thread_id_for_workspace_any(&workspace) {
            // Only a thread of this Space (or one it references) can be
            // activated here; activate_workspace_thread's guard silently
            // drops anything else, which used to make these clicks no-ops.
            let home_space = crate::workspace_threads::thread_space_id(&thread_id);
            let in_this_space = home_space.as_deref() == Some(self.active_space_id.as_str());
            let referenced = !in_this_space
                && crate::workspace_threads::thread_ref_exists(&self.active_space_id, &thread_id);
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
                    .gui_window_for_recovery_slot(FrontendRecoverySlot::Window(target_owner_id))
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
                        target_term_window.activate_workspace_thread(thread_id, &target_window);
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

/// What `fitted_description` settled on for one agent, and for which
/// description, width and font.
pub(crate) struct FittedDescription {
    msg: String,
    width: usize,
    font: wezterm_font::LoadedFontId,
    /// `None`: the description is drawn as it is.
    fitted: Option<Rc<str>>,
}

/// One entry of the list as the view menu arranges it: a group's heading
/// with how many it holds, or an agent's row by its index.
enum PanelEntry {
    Header { label: String, count: usize },
    Agent(usize),
}

/// Where an agent falls in the view's grouping, as a sort key: the states
/// in the order they need the user, or threads by name with the agents
/// that have none last -- and by workspace too, so two threads that share
/// a name (one here, one on a server) stay two groups. Without grouping
/// every agent is in one.
fn group_order(
    agent: &agent_status::AgentPaneStatus,
    group_by: crate::native_settings::AgentsGroupBy,
) -> (u8, String, String) {
    use crate::native_settings::AgentsGroupBy;
    use crate::workspace_threads::WorkspaceThreadWorkStatus as Status;
    match group_by {
        AgentsGroupBy::State => {
            let rank = match agent.work_status() {
                Status::NeedsAttention => 0,
                Status::Running => 1,
                Status::FinishedUnseen => 2,
                // Undetected agents go with the idle ones in the Show
                // filter, but under a heading of their own: "Idle 3" over
                // three rows each saying Unknown would be wrong.
                Status::Idle if agent.shown_state() == agent_status::ShownState::Unknown => 4,
                Status::Idle => 3,
            };
            (rank, String::new(), String::new())
        }
        AgentsGroupBy::Thread => (
            u8::from(agent.place.is_empty()),
            agent.place.to_lowercase(),
            agent.workspace.clone(),
        ),
        AgentsGroupBy::None => (0, String::new(), String::new()),
    }
}

/// The heading over a group, in the words the left sidebar's status filter
/// uses for the states.
fn group_label(
    agent: &agent_status::AgentPaneStatus,
    group_by: crate::native_settings::AgentsGroupBy,
) -> String {
    use crate::native_settings::AgentsGroupBy;
    use crate::workspace_threads::WorkspaceThreadWorkStatus as Status;
    match group_by {
        AgentsGroupBy::Thread if !agent.place.is_empty() => agent.place.clone(),
        AgentsGroupBy::Thread | AgentsGroupBy::None => crate::i18n::tr("right-agents-group-other"),
        AgentsGroupBy::State => crate::i18n::tr(match agent.work_status() {
            Status::NeedsAttention => "menu-status-attention",
            Status::Running => "menu-status-running",
            Status::FinishedUnseen => "menu-status-done",
            Status::Idle if agent.shown_state() == agent_status::ShownState::Unknown => {
                "right-agents-state-unknown"
            }
            Status::Idle => "menu-status-idle",
        }),
    }
}

/// Put agents already in `sort_for_display` order into the view's: by
/// group, then -- when asked -- whatever changed state last first. The sort
/// is stable, so the fixed order holds within a group otherwise.
fn order_for_view(
    agents: &mut [agent_status::AgentPaneStatus],
    view: &crate::native_settings::NativeAgentsPanelView,
) {
    let group_by = view.group_by();
    let recent = view.sort_by() == crate::native_settings::AgentsSortBy::Recent;
    agents.sort_by(|a, b| {
        group_order(a, group_by)
            .cmp(&group_order(b, group_by))
            .then_with(|| {
                if recent {
                    b.since_unix.cmp(&a.since_unix)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
    });
}

/// The list as it is drawn: a heading before each group's first agent,
/// none when the view does not group.
fn panel_entries(
    agents: &[agent_status::AgentPaneStatus],
    group_by: crate::native_settings::AgentsGroupBy,
) -> Vec<PanelEntry> {
    let grouped = group_by != crate::native_settings::AgentsGroupBy::None;
    let mut entries = Vec::with_capacity(agents.len() + 4);
    let mut index = 0;
    while index < agents.len() {
        let key = group_order(&agents[index], group_by);
        let end = agents[index..]
            .iter()
            .position(|agent| group_order(agent, group_by) != key)
            .map_or(agents.len(), |len| index + len);
        if grouped {
            entries.push(PanelEntry::Header {
                label: group_label(&agents[index], group_by),
                count: end - index,
            });
        }
        entries.extend((index..end).map(PanelEntry::Agent));
        index = end;
    }
    entries
}

/// Which of a row's optional lines it has, and how tall that makes it.
struct AgentRowLayout {
    height: usize,
    /// The description, with the waiting reason in front of it.
    message: bool,
    progress: Option<u8>,
    /// The sub-tasks the folded line counts; none when they are not shown.
    subtask_count: usize,
    /// While the list is open: the sub-task lines it lists, then how many
    /// more there are.
    subtasks: usize,
    more_subtasks: usize,
}

impl AgentRowLayout {
    fn of(
        agent: &agent_status::AgentPaneStatus,
        show: crate::native_settings::NativeAgentStatusDisplay,
        open: bool,
        base_height: usize,
        line: usize,
        bar: usize,
    ) -> Self {
        let Some(report) = agent.report.as_ref() else {
            return Self {
                height: base_height,
                message: false,
                progress: None,
                subtask_count: 0,
                subtasks: 0,
                more_subtasks: 0,
            };
        };
        let message = (show.description && report.msg.is_some())
            || (show.reason && report.kind.is_some());
        let progress = report.progress.filter(|_| show.progress);
        let subtask_count = if show.subtasks {
            report.children.len()
        } else {
            0
        };
        let listed = if open { subtask_count } else { 0 };
        let subtasks = listed.min(AGENT_SUBTASKS_SHOWN);
        let more_subtasks = listed - subtasks;
        let height = base_height
            + if message { line } else { 0 }
            + if progress.is_some() { bar } else { 0 }
            + (usize::from(subtask_count > 0) + subtasks + usize::from(more_subtasks > 0)) * line;
        Self {
            height,
            message,
            progress,
            subtask_count,
            subtasks,
            more_subtasks,
        }
    }
}

/// How many parts a word of a description has as a path; 0 when it is not
/// one. A path starts at a root (`/a/b`, `~/a`, `./a`, `../a`) or runs to
/// three parts (`src/app/main.rs`). Two parts and no root is a ratio or a
/// choice as often as a path -- `3/12`, `read/write` -- and numbers alone,
/// a date among them, never are; nor is a URL.
fn path_parts(word: &str) -> usize {
    if !word.contains('/') || word.contains("://") {
        return 0;
    }
    let parts: Vec<&str> = word.split('/').filter(|part| !part.is_empty()).collect();
    let rooted = ["/", "~/", "./", "../"]
        .iter()
        .any(|root| word.starts_with(root));
    let numbers_only = parts
        .iter()
        .all(|part| part.chars().all(|c| c.is_ascii_digit() || c.is_ascii_punctuation()));
    if parts.len() < 2 || numbers_only || (!rooted && parts.len() < 3) {
        return 0;
    }
    parts.len()
}

/// `text` with every path in it cut back to its last `keep` parts, or
/// `None` when that changes nothing.
fn shorten_paths(text: &str, keep: usize) -> Option<String> {
    let mut changed = false;
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let word_start = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        out.push_str(&rest[..word_start]);
        rest = &rest[word_start..];
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let word = &rest[..word_end];
        rest = &rest[word_end..];
        let parts = path_parts(word);
        if parts > keep {
            let tail: Vec<&str> = word.split('/').filter(|part| !part.is_empty()).collect();
            out.push_str(&tail[parts - keep..].join("/"));
            changed = true;
        } else {
            out.push_str(word);
        }
    }
    changed.then_some(out)
}

/// How far from a line's right edge text starts to fade, when the line does
/// not fit: about four characters.
const AGENT_TEXT_FADE: f32 = 64.0;

/// How far the shimmer lights either side of its centre, in line heights
/// (about six characters).
const SHIMMER_HALF_WIDTH: f32 = 2.5;
/// How far the shimmer moves each spinner frame, in line heights (about
/// one and a half characters). A fixed pace: a longer line takes longer to
/// cross, rather than being crossed in bigger jumps.
const SHIMMER_STEP: f32 = 0.6;
/// Colours from the text's to the shimmer's light, ends included.
const SHIMMER_RAMP_STEPS: usize = 9;

/// Where the band of light crossing a working program's description is.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ShimmerBand {
    /// From the left edge of the text.
    centre: f32,
    half_width: f32,
}

impl ShimmerBand {
    /// `frames` spinner frames in, across text `travel` wide: the band
    /// enters from the left, crosses at a fixed pace and leaves on the
    /// right, then starts again. It is off the text at both ends, so a pass
    /// starts and ends with the text unlit. Whole-number arithmetic on the
    /// frame count, so the band never jumps however long the clock runs.
    fn at(frames: u128, travel: f32, line_height: f32) -> Self {
        let half_width = (line_height * SHIMMER_HALF_WIDTH).max(1.0);
        let step = (line_height * SHIMMER_STEP).max(1.0);
        let pass = ((travel.max(0.0) + half_width * 2.0) / step)
            .ceil()
            .max(1.0) as u128;
        Self {
            centre: (frames % pass) as f32 * step - half_width,
            half_width,
        }
    }

    /// How lit the glyph whose middle is `x` from the text's left edge is:
    /// 0 outside the band, 1 at its centre, eased between so it has no edge.
    fn strength(self, x: f32) -> f32 {
        let t = (1.0 - (x - self.centre).abs() / self.half_width).max(0.0);
        t * t * (3.0 - 2.0 * t)
    }
}

/// The icon a state is shown with: glyph, colour, and whether it spins.
pub(crate) fn agent_state_icon(
    state: agent_status::ShownState,
    chrome: UiPalette,
    muted_fg: LinearRgba,
) -> Option<(SvgIcon, LinearRgba, bool)> {
    use agent_status::ShownState;
    match state {
        ShownState::Working => Some((SvgIcon::LoaderCircle, AGENT_WORKING_COLOR, true)),
        ShownState::Blocked => Some((SvgIcon::CircleAlert, AGENT_BLOCKED_COLOR, false)),
        ShownState::Error => Some((SvgIcon::CircleX, chrome.danger, false)),
        ShownState::Done => Some((SvgIcon::CircleCheck, AGENT_DONE_COLOR, false)),
        ShownState::Idle => Some((SvgIcon::CircleCheck, muted_fg, false)),
        ShownState::Unknown => None,
    }
}

pub(crate) fn agent_state_key(state: agent_status::ShownState) -> &'static str {
    use agent_status::ShownState;
    match state {
        ShownState::Working => "right-agents-state-working",
        ShownState::Blocked => "right-agents-state-blocked",
        ShownState::Error => "right-agents-state-error",
        ShownState::Done => "right-agents-state-done",
        ShownState::Idle => "right-agents-state-idle",
        ShownState::Unknown => "right-agents-state-unknown",
    }
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
    use super::{agent_row_visible_band, AgentRowBand, ShimmerBand};

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

    #[test]
    fn paths_are_cut_back_to_their_last_parts() {
        use super::shorten_paths;
        let msg = "Reading /private/tmp/claude/scratch/agent-status-notes.md";
        assert_eq!(
            shorten_paths(msg, 2).as_deref(),
            Some("Reading scratch/agent-status-notes.md")
        );
        assert_eq!(
            shorten_paths(msg, 1).as_deref(),
            Some("Reading agent-status-notes.md")
        );
        // Every path in the line, the words between left alone.
        assert_eq!(
            shorten_paths("cp  src/a/b.rs ~/x/y/c.rs", 1).as_deref(),
            Some("cp  b.rs c.rs")
        );
        // Nothing to cut: no path, a path already short, a URL.
        assert_eq!(shorten_paths("Running cargo test", 1), None);
        assert_eq!(shorten_paths("Editing notes.md", 1), None);
        assert_eq!(shorten_paths("Fetching https://example.com/a/b", 1), None);
        // Ratios, choices and dates are not paths, beside one that is.
        assert_eq!(
            shorten_paths("Running tests (3/12) in src/app/tests/foo_test.rs", 1).as_deref(),
            Some("Running tests (3/12) in foo_test.rs")
        );
        assert_eq!(shorten_paths("Step 2/5: fix read/write path", 1), None);
        assert_eq!(shorten_paths("Done 3/7 tasks on 2026/10/10", 1), None);
    }

    #[test]
    fn shimmer_crosses_at_one_pace_whatever_the_length() {
        let line = 20.0;
        for travel in [60.0, 600.0] {
            let first = ShimmerBand::at(0, travel, line);
            let next = ShimmerBand::at(1, travel, line);
            assert_eq!(next.centre - first.centre, line * super::SHIMMER_STEP);
        }
        let pass = |travel: f32| {
            (1..10_000u128)
                .find(|&f| {
                    ShimmerBand::at(f, travel, line).centre
                        < ShimmerBand::at(f - 1, travel, line).centre
                })
                .unwrap()
        };
        assert!(
            pass(600.0) > pass(60.0) * 4,
            "a longer line takes longer to cross"
        );
    }

    #[test]
    fn shimmer_starts_and_ends_off_the_text() {
        let (line, travel) = (20.0, 300.0);
        let start = ShimmerBand::at(0, travel, line);
        assert_eq!(start.strength(0.0), 0.0, "nothing lit as a pass begins");
        let last = (0..1000u128)
            .map(|f| ShimmerBand::at(f, travel, line))
            .take_while(|band| band.centre >= start.centre)
            .last()
            .unwrap();
        assert_eq!(last.strength(travel), 0.0, "nothing lit as a pass ends");
        // And the clock running for years changes nothing about that.
        let late = ShimmerBand::at(u64::MAX as u128 * 7, travel, line);
        assert!(late.centre >= start.centre && late.centre <= travel + start.half_width);
    }

    #[test]
    fn shimmer_is_brightest_at_its_centre() {
        let band = ShimmerBand::at(10, 300.0, 20.0);
        assert_eq!(band.strength(band.centre), 1.0);
        assert!(band.strength(band.centre + band.half_width / 2.0) < 1.0);
        assert_eq!(band.strength(band.centre + band.half_width), 0.0);
        assert_eq!(band.strength(band.centre - band.half_width * 3.0), 0.0);
    }

    /// Sub-tasks fold to the one line that counts them: only an open list
    /// adds its lines, and with sub-tasks switched off there is no line.
    #[test]
    fn sub_tasks_fold_to_one_line_until_opened() {
        use super::AgentRowLayout;
        use crate::agent_status::{
            AgentEvidence, AgentPaneStatus, AgentState, ProgramReport, ProgramReportState,
        };
        use crate::native_settings::NativeAgentStatusDisplay;
        use thinkterm_proto::ProgramReportChild;

        let child = |n: usize| ProgramReportChild {
            id: format!("task-{n}"),
            state: ProgramReportState::Working,
            kind: None,
            progress: None,
            title: None,
            msg: None,
        };
        let agent = AgentPaneStatus {
            pane_id: 1,
            agent_id: "claude".to_string(),
            state: AgentState::Working,
            evidence: AgentEvidence::Report,
            session_id: None,
            title: String::new(),
            window_id: None,
            place: String::new(),
            workspace: String::new(),
            machine: String::new(),
            report: Some(ProgramReport {
                state: ProgramReportState::Working,
                kind: None,
                progress: None,
                app: None,
                title: None,
                msg: None,
                children: (0..7).map(child).collect(),
            }),
            since_unix: 0,
        };
        let show = NativeAgentStatusDisplay::default();
        let (base, line) = (40, 10);

        let folded = AgentRowLayout::of(&agent, show, false, base, line, 4);
        assert_eq!(
            (folded.subtask_count, folded.subtasks, folded.more_subtasks),
            (7, 0, 0)
        );
        assert_eq!(folded.height, base + line);

        let open = AgentRowLayout::of(&agent, show, true, base, line, 4);
        assert_eq!((open.subtasks, open.more_subtasks), (5, 2));
        assert_eq!(open.height, base + line * 7, "fold line, five, and the rest");

        let hidden = NativeAgentStatusDisplay {
            subtasks: false,
            ..show
        };
        let off = AgentRowLayout::of(&agent, hidden, true, base, line, 4);
        assert_eq!((off.subtask_count, off.height), (0, base));
    }

    fn agent_at(
        pane_id: usize,
        state: crate::agent_status::AgentState,
        place: &str,
        since_unix: u64,
    ) -> crate::agent_status::AgentPaneStatus {
        crate::agent_status::AgentPaneStatus {
            pane_id: pane_id as mux::pane::PaneId,
            agent_id: "claude".to_string(),
            state,
            evidence: crate::agent_status::AgentEvidence::Screen,
            session_id: None,
            title: String::new(),
            window_id: None,
            place: place.to_string(),
            workspace: String::new(),
            machine: String::new(),
            report: None,
            since_unix,
        }
    }

    /// Rows as the view lists them: each heading's count, then the panes
    /// under it.
    fn listed(
        agents: &mut Vec<crate::agent_status::AgentPaneStatus>,
        group_by: &str,
        sort_by: &str,
    ) -> Vec<String> {
        use super::{order_for_view, panel_entries, PanelEntry};
        let view = crate::native_settings::NativeAgentsPanelView {
            group_by: group_by.to_string(),
            sort_by: sort_by.to_string(),
            ..Default::default()
        };
        crate::agent_status::sort_for_display(agents);
        order_for_view(agents, &view);
        panel_entries(agents, view.group_by())
            .into_iter()
            .map(|entry| match entry {
                PanelEntry::Header { count, .. } => format!("#{}", count),
                PanelEntry::Agent(row) => agents[row].pane_id.to_string(),
            })
            .collect()
    }

    /// Grouped by state, the agents that need the user lead, then those
    /// working, done and idle, each under a heading that counts them.
    #[test]
    fn the_view_groups_by_state_with_what_needs_the_user_first() {
        use crate::agent_status::AgentState::{Blocked, Error, Idle, Working};
        let mut agents = vec![
            agent_at(1, Working, "a", 10),
            agent_at(2, Idle, "a", 20),
            agent_at(3, Blocked, "b", 30),
            agent_at(4, Error, "a", 40),
            agent_at(5, Working, "b", 50),
        ];
        assert_eq!(
            listed(&mut agents, "", ""),
            ["#2", "4", "3", "#2", "1", "5", "#1", "2"]
        );
        // Most recent first changes the order inside a group, never across.
        assert_eq!(
            listed(&mut agents, "state", "recent"),
            ["#2", "4", "3", "#2", "5", "1", "#1", "2"]
        );
    }

    #[test]
    fn the_view_groups_by_thread_or_not_at_all() {
        use crate::agent_status::AgentState::{Blocked, Working};
        let mut agents = vec![
            agent_at(1, Working, "beta", 10),
            agent_at(2, Blocked, "", 20),
            agent_at(3, Working, "alpha", 30),
            agent_at(4, Working, "beta", 40),
        ];
        // Threads by name; agents with no thread last, under "Other".
        assert_eq!(
            listed(&mut agents, "thread", "fixed"),
            ["#1", "3", "#2", "1", "4", "#1", "2"]
        );
        // Two threads that share a name -- one here, one on a server -- are
        // two groups, not one.
        let mut twins = vec![
            agent_at(1, Working, "app · main", 10),
            agent_at(2, Working, "app · main", 20),
            agent_at(3, Working, "app · main", 30),
        ];
        twins[0].workspace = "local-main".to_string();
        twins[1].workspace = "remote-main".to_string();
        twins[2].workspace = "local-main".to_string();
        assert_eq!(
            listed(&mut twins, "thread", "fixed"),
            ["#2", "1", "3", "#1", "2"]
        );
        // No headings at all; a hand-edited key nobody knows falls back.
        assert_eq!(listed(&mut agents, "none", "fixed"), ["2", "3", "1", "4"]);
        assert_eq!(listed(&mut agents, "bogus", "bogus")[0], "#1");
    }

}
