//! Hover surfaces: the tag an icon-only button or a cut-short row shows,
//! and the cards a thread row and an Agents panel row show. One look for
//! all of them, the right-click menu's: its card colours, a hairline border
//! and the soft two-layer shadow. They appear after a pause; once one has
//! shown, the next one shows at once, as on the Mac.
//!
//! Text goes through the sidebar's cached shaping, so a surface that stays
//! up while a spinner repaints the window shapes nothing again. It wraps
//! rather than being cut, and whatever still runs over fades, as in the
//! Agents panel.

use crate::agent_status::{self, AgentPaneStatus, AgentPanelAction, ShownState};
use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::ui::agent_panel::{
    agent_row_title, agent_state_icon, agent_state_key, agent_title, brand_of, working_subtasks,
    AGENT_PROGRESS_HEIGHT,
};
use crate::termwindow::{HoverTooltip, TermWindow, UIItem, UIItemType, TOOLTIP_DELAY};
use crate::ui::{DrawContext, UiPalette};
use crate::utilsprites::RenderMetrics;
use anyhow::Context;
use mux::pane::PaneId;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wezterm_font::LoadedFont;
use window::color::LinearRgba;
use window::{Modifiers, UIKeyCapRendering};

const TAG_RADIUS: f32 = 16.0;
const CARD_RADIUS: f32 = 24.0;
const TAG_PAD_X: f32 = 18.0;
const TAG_PAD_Y: f32 = 10.0;
const CARD_PAD_X: f32 = 28.0;
const CARD_PAD_Y: f32 = 24.0;
/// A tag's widest before it wraps, and the lines it wraps to.
const TAG_MAX_WIDTH: f32 = 600.0;
const TAG_LINES: usize = 3;
const CARD_WIDTH: f32 = 560.0;
/// Lines an agent card gives its title and its description.
const CARD_TITLE_LINES: usize = 2;
const CARD_MESSAGE_LINES: usize = 4;
/// Between a surface and what it describes, and between a card's parts.
const GAP: f32 = 16.0;
/// Between a tag's label and its shortcut.
const SHORTCUT_GAP: f32 = 14.0;
/// A tag's shortcut against its label's size.
const SHORTCUT_SCALE: f64 = 0.86;
/// Between a card and the row it describes, clear of the sidebar's edge.
const CARD_GAP: f32 = 22.0;
/// Between two lines of wrapped text.
const LINE_SPACING: f32 = 2.0;

/// The fonts and colours every hover surface is set in.
struct HoverStyle {
    body: Rc<LoadedFont>,
    strong: Rc<LoadedFont>,
    metrics: RenderMetrics,
    strong_metrics: RenderMetrics,
    line: f32,
    palette: UiPalette,
    font_size: f64,
}

impl TermWindow {
    /// The hover surface for whatever the pointer rests on, once it has
    /// rested long enough.
    pub(crate) fn paint_hover_tooltip(&mut self) -> anyhow::Result<()> {
        let Some(hover) = self.hover_tooltip.clone().filter(|hover| !hover.dismissed) else {
            return Ok(());
        };
        if !hover.warm {
            let remaining = TOOLTIP_DELAY.saturating_sub(hover.since.elapsed());
            if !remaining.is_zero() {
                // Ask for the frame that will find the delay elapsed. This has
                // to happen here and not where the pointer arrived: paint_impl
                // clears has_animation on entry, so a deadline registered from
                // the mouse handler is wiped before anything can read it.
                self.update_next_frame_time(Some(Instant::now() + remaining));
                return Ok(());
            }
        }
        let painted = self.paint_hover(&hover)?;
        if let Some(hover) = self.hover_tooltip.as_mut() {
            hover.blank = !painted;
        }
        Ok(())
    }

    /// Paints `hover`'s surface; false when there turns out to be none.
    fn paint_hover(&mut self, hover: &HoverTooltip) -> anyhow::Result<bool> {
        // Re-check the dynamic conditions at paint time: a rename started
        // from a key assignment (no mouse event) or a row whose label no
        // longer overflows must suppress an already-armed tag.
        if !self.hover_tooltip_allowed(&hover.item.item_type) {
            return Ok(false);
        }
        let Some(item) = self.hover_anchor(&hover.item.item_type) else {
            return Ok(false);
        };
        if let Some(pane_id) = agent_card_pane(&item.item_type) {
            // Borrowed from the shared snapshot rather than copied each frame.
            let snapshot = agent_status::agent_panes();
            let Some(agent) = snapshot.iter().find(|agent| agent.pane_id == pane_id) else {
                return Ok(false);
            };
            let style = self.hover_style()?;
            self.paint_agent_hover_card(&item, agent, &style)?;
            return Ok(true);
        }
        let agents = crate::termwindow::thread_card_agents(&item.item_type);
        if !agents.is_empty() {
            let style = self.hover_style()?;
            self.paint_thread_hover_card(&item, &agents, &style)?;
            return Ok(true);
        }
        let Some(label) = crate::termwindow::tooltip_label_for(&item.item_type) else {
            return Ok(false);
        };
        let style = self.hover_style()?;
        self.paint_hover_tag(&item, &label, hover.shortcut.as_deref(), &style)?;
        Ok(true)
    }

    /// The item a surface sits beside, as this frame laid it out: none when
    /// it is gone, with its sidebar hidden or its row scrolled away, and
    /// none when it is no longer under the pointer -- a list that re-sorted
    /// under a still pointer has put another row there. An agent card stays
    /// beside the agent's row while the pointer is on the row's sub-task
    /// line.
    fn hover_anchor(&self, item_type: &UIItemType) -> Option<UIItem> {
        let found = match agent_card_pane(item_type) {
            Some(pane_id) => self
                .ui_items
                .iter()
                .find(|item| agent_row_pane(&item.item_type) == Some(pane_id)),
            None => self
                .ui_items
                .iter()
                .find(|item| item.item_type == *item_type),
        };
        found
            .filter(|item| self.is_pointer_over_ui_rect(item.x, item.y, item.width, item.height))
            .cloned()
    }

    /// The shortcut a tag shows after its label: the first chord bound to
    /// the item's action, the platform's own modifier first, as the
    /// keyboard settings label it.
    pub(crate) fn hover_shortcut(&self, item_type: &UIItemType) -> Option<String> {
        let action = crate::termwindow::tooltip_action_for(item_type)?;
        let mut chords = self
            .input_map
            .locate_app_wide_key_assignment(&action)
            .into_iter()
            .map(|(key, mods)| crate::inputmap::canonical_chord(&key, mods))
            .collect::<Vec<_>>();
        let native =
            |mods: &Modifiers| mods.contains(Modifiers::SUPER) == cfg!(target_os = "macos");
        chords.sort_by(|(a_key, a_mods), (b_key, b_mods)| {
            native(b_mods)
                .cmp(&native(a_mods))
                .then(a_mods.bits().cmp(&b_mods.bits()))
                .then(a_key.cmp(b_key))
        });
        let (key, mods) = chords.into_iter().next()?;
        let rendering = self.config.ui_key_cap_rendering;
        let label = crate::inputmap::chord_label(&key, mods, rendering);
        // Set tight, as the Mac's menus write it: ⌘P, not ⌘ P.
        Some(if rendering == UIKeyCapRendering::AppleSymbols {
            label.replace(' ', "")
        } else {
            label
        })
    }

    fn hover_style(&self) -> anyhow::Result<HoverStyle> {
        let settings = crate::native_settings::load_shared();
        let font_size = crate::native_settings::home_font_size(&settings);
        let body = self
            .fonts
            .title_font_with_size_and_weight(font_size, 400)
            .context("hover font")?;
        let strong = self
            .fonts
            .title_font_with_size_and_weight(font_size, 600)
            .context("hover heading font")?;
        let metrics = RenderMetrics::with_font_metrics(&body.metrics());
        let strong_metrics = RenderMetrics::with_font_metrics(&strong.metrics());
        let mut palette = self.chrome().as_menu_card();
        crate::native_settings::apply_ui_colors(&mut palette, self.config.ui_colors.as_ref());
        Ok(HoverStyle {
            line: metrics.cell_size.height as f32,
            body,
            strong,
            metrics,
            strong_metrics,
            palette,
            font_size,
        })
    }

    fn paint_hover_surface(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        style: &HoverStyle,
        rect: euclid::Rect<f32, window::PixelUnit>,
        radius: f32,
    ) -> anyhow::Result<()> {
        let render_state = self.render_state.as_ref().context("hover render state")?;
        let ctx = DrawContext::new(render_state, self.dimensions, &style.metrics);
        let shadow = if style.palette.is_dark() { 0.58 } else { 0.43 };
        ctx.draw_elevated_surface(
            layers,
            0,
            rect,
            style.palette.control_bg,
            style.palette.control_border,
            LinearRgba::with_components(0.0, 0.0, 0.0, shadow),
            radius,
        )
        .context("hover surface")
    }

    /// `text` broken into at most `max_lines` lines no wider than `width`,
    /// as byte ranges into it: before a word that would not fit, after a `/`
    /// so a path breaks between directories, or -- with neither near, or in
    /// text without spaces -- right where it runs over. The last line keeps
    /// the rest, for the painter to fade if it still runs over.
    fn wrap_hover_text(
        &self,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
        width: f32,
        max_lines: usize,
    ) -> anyhow::Result<Vec<Range<usize>>> {
        let (shaped, _) = self.cached_ui_shape(font, metrics, text)?;
        let glyphs: Vec<(usize, f32)> = shaped
            .iter()
            .map(|info| (info.cluster, info.glyph.x_advance.get() as f32))
            .collect();
        Ok(wrap_glyphs(text, &glyphs, width, max_lines))
    }

    fn paint_hover_tag(
        &mut self,
        item: &UIItem,
        label: &str,
        shortcut: Option<&str>,
        style: &HoverStyle,
    ) -> anyhow::Result<()> {
        let window_w = self.dimensions.pixel_width as f32;
        let window_h = self.dimensions.pixel_height as f32;
        let max_width = self.ui_f32(TAG_MAX_WIDTH).min(window_w * 0.6).max(80.0);
        let lines =
            self.wrap_hover_text(&style.body, &style.metrics, label, max_width, TAG_LINES)?;
        let mut text_width = 0.0f32;
        for range in &lines {
            text_width = text_width.max(self.cached_ui_text_advance(
                &style.body,
                &style.metrics,
                &label[range.clone()],
            )?);
        }
        let text_width = text_width.min(max_width).ceil();
        // The shortcut sits after a one-line label, a step smaller and
        // dimmer, on the label's baseline: the key symbols come from a
        // fallback font and at full size outweigh the words.
        let shortcut = match shortcut {
            Some(keys) if lines.len() == 1 => {
                let font = self
                    .fonts
                    .title_font_with_size_and_weight(style.font_size * SHORTCUT_SCALE, 400)
                    .context("hover shortcut font")?;
                let metrics = RenderMetrics::with_font_metrics(&font.metrics());
                let width = self.cached_ui_text_advance(&font, &metrics, keys)?.ceil();
                Some((keys, font, metrics, width))
            }
            _ => None,
        };
        let key_gap = self.ui_f32(SHORTCUT_GAP);
        let key_width = shortcut
            .as_ref()
            .map_or(0.0, |(_, _, _, width)| key_gap + width);
        let (pad_x, pad_y, gap) = (
            self.ui_f32(TAG_PAD_X),
            self.ui_f32(TAG_PAD_Y),
            self.ui_f32(GAP) / 2.0,
        );
        let spacing = self.ui_f32(LINE_SPACING);
        let tip_w = text_width + key_width + pad_x * 2.0;
        let tip_h = style.line * lines.len() as f32
            + spacing * lines.len().saturating_sub(1) as f32
            + pad_y * 2.0;

        // Above the button. Beside it would sit on top of the neighbouring
        // buttons in the same row -- these are laid out horizontally, so the
        // only free direction is up. Below when there is no room above.
        // Full-width list rows left-align their tag; icon buttons centre it.
        let (bx, by, bw, bh) = (
            item.x as f32,
            item.y as f32,
            item.width as f32,
            item.height as f32,
        );
        let left_align = crate::termwindow::tooltip_left_aligns(&item.item_type);
        let x = crate::termwindow::tooltip_anchor_x(bx, bw, tip_w, window_w, left_align);
        // The Library's Spaces tab stands level with the window's own
        // buttons, with nothing under it: its tag goes there.
        let below_first = matches!(
            item.item_type,
            UIItemType::Library(crate::termwindow::ui::library::LibraryItem::Spaces)
        );
        let y = if !below_first && by - gap - tip_h >= 0.0 {
            by - gap - tip_h
        } else {
            (by + bh + gap).min((window_h - tip_h).max(0.0))
        };

        let gl_state = self.render_state.as_ref().context("hover render state")?;
        let layer = gl_state
            .layer_for_zindex(crate::termwindow::TOOLTIP_ZINDEX)
            .context("hover tooltip layer")?;
        let mut layers = layer.quad_allocator();
        self.paint_hover_surface(
            &mut layers,
            style,
            euclid::rect(x, y, tip_w, tip_h),
            self.ui_f32(TAG_RADIUS),
        )?;
        let mut line_y = y + pad_y;
        for range in &lines {
            self.paint_agent_text(
                &mut layers,
                &style.body,
                style.metrics,
                &label[range.clone()],
                (x + pad_x).round() as usize,
                line_y.round() as usize,
                text_width as usize,
                style.palette.text,
            )?;
            line_y += style.line + spacing;
        }
        if let Some((keys, font, metrics, width)) = shortcut {
            let baseline = |m: &RenderMetrics| m.cell_size.height as f32 + m.descender.get() as f32;
            let top = y + pad_y + baseline(&style.metrics) - baseline(&metrics);
            self.paint_agent_text(
                &mut layers,
                &font,
                metrics,
                keys,
                (x + pad_x + text_width + key_gap).round() as usize,
                top.round() as usize,
                width as usize + 1,
                style.palette.muted_text,
            )?;
        }
        Ok(())
    }

    /// Where a card goes beside `item`: on the `prefer_right` side when it
    /// fits there, the other side when not; top-aligned with the item and
    /// kept inside the window.
    fn hover_card_origin(
        &self,
        item: &UIItem,
        card_w: f32,
        card_h: f32,
        prefer_right: bool,
    ) -> (f32, f32) {
        let window_w = self.dimensions.pixel_width as f32;
        let window_h = self.dimensions.pixel_height as f32;
        // Rows sit inside their sidebar's padding: a card a tag's distance
        // from the row would overlap the sidebar's edge.
        let gap = self.ui_f32(CARD_GAP);
        let right = item.x as f32 + item.width as f32 + gap;
        let left = item.x as f32 - gap - card_w;
        let on_right = if prefer_right {
            right + card_w <= window_w || left < 0.0
        } else {
            left < 0.0 && right + card_w <= window_w
        };
        // Where neither side has room, inside the window over the item.
        let x = (if on_right { right } else { left }).clamp(0.0, (window_w - card_w).max(0.0));
        let y = (item.y as f32).min(window_h - card_h).max(0.0);
        (x, y)
    }

    /// `in_active_pane`: the agent is in the pane the user is in.
    #[allow(clippy::too_many_arguments)]
    fn paint_hover_state_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        state: ShownState,
        in_active_pane: bool,
        x: f32,
        y: f32,
        size: f32,
        style: &HoverStyle,
    ) -> anyhow::Result<()> {
        let palette = style.palette;
        // The ring is these icons; the other styles draw the state as the
        // sidebar does.
        let status_style = crate::native_settings::agents_status();
        // A finished result keeps the card's own tick, at the card's icon
        // size, whatever the style.
        if status_style != crate::native_settings::NativeThreadStatus::Ring
            && state != ShownState::Done
        {
            let Some((mark, color)) =
                crate::termwindow::ui::agent_panel::agent_state_mark(state, palette, palette.muted_text)
            else {
                return Ok(());
            };
            let color = crate::termwindow::ui::agent_panel::agent_state_color_where(
                state,
                color,
                in_active_pane,
            );
            // As large as the sidebar draws it, centred where the icon goes.
            let mark_size = (self
                .ui_px(crate::termwindow::ui::sidebar::SESSION_STATUS_ICON_SIZE)
                as f32)
                .min(size);
            let inset = (size - mark_size) / 2.0;
            return self.paint_thread_mark(
                layers,
                status_style,
                mark,
                (x + inset).round(),
                (y + inset).round(),
                mark_size.round(),
                color,
            );
        }
        let Some((icon, color, spinning)) = agent_state_icon(state, palette, palette.muted_text)
        else {
            return Ok(());
        };
        let color = crate::termwindow::ui::agent_panel::agent_state_color_where(
            state,
            color,
            in_active_pane,
        );
        let (x, y, size) = (
            x.round() as usize,
            y.round() as usize,
            size.round() as usize,
        );
        if spinning {
            self.paint_spinning_ui_icon(layers, 2, icon, x, y, size, color)
        } else {
            self.paint_sidebar_icon(layers, icon, x, y, size, color)
        }
    }

    /// The sizes both cards are laid out with.
    fn card_frame(&self, style: &HoverStyle) -> CardFrame {
        let window_w = self.dimensions.pixel_width as f32;
        CardFrame {
            width: self.ui_f32(CARD_WIDTH).min(window_w * 0.45),
            pad_x: self.ui_f32(CARD_PAD_X),
            pad_y: self.ui_f32(CARD_PAD_Y),
            gap: self.ui_f32(GAP),
            spacing: self.ui_f32(LINE_SPACING),
            line: style.line,
        }
    }

    fn paint_hover_separator(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        x: f32,
        y: f32,
        width: f32,
        style: &HoverStyle,
    ) -> anyhow::Result<()> {
        self.fill_rounded_rectangle(
            layers,
            1,
            euclid::rect(x, y.round(), width, 1.0),
            style.palette.separator,
            0.0,
        )
        .context("hover separator")
    }

    /// A thread row's card: the thread's name and how many agents it holds,
    /// then each agent in two lines -- its mark, title, pane and state icon,
    /// then its state in words, how far it has got, what it waits for and
    /// what it says -- parted by hairlines. Beside the row rather than above
    /// it: the card is taller than a tag, and the rows above are the ones
    /// being read.
    fn paint_thread_hover_card(
        &mut self,
        item: &UIItem,
        agents: &[AgentPaneStatus],
        style: &HoverStyle,
    ) -> anyhow::Result<()> {
        let show = crate::native_settings::agent_status_display();
        let CardFrame {
            width: card_w,
            pad_x,
            pad_y,
            gap,
            spacing,
            line,
        } = self.card_frame(style);
        let mark = line;
        let text_x = pad_x + mark + gap / 2.0;
        let text_w = card_w - text_x - pad_x - mark - gap / 2.0;
        let entry_h = line * 2.0 + spacing;
        let header_h = line;
        // As many agents as the window has room for, in order, and a last
        // line saying how many more there are.
        let head_h = pad_y * 2.0 + header_h + gap + 1.0 + gap;
        let rows_h = |rows: usize| entry_h * rows as f32 + gap * rows.saturating_sub(1) as f32;
        let more_h = gap + line;
        let room = self.dimensions.pixel_height as f32 - gap * 2.0;
        let card_h_for =
            |rows: usize| head_h + rows_h(rows) + if rows < agents.len() { more_h } else { 0.0 };
        let rows = (1..=agents.len())
            .rev()
            .find(|&rows| card_h_for(rows) <= room)
            .unwrap_or(1)
            .min(agents.len());
        let card_h = card_h_for(rows);
        let (x, y) = self.hover_card_origin(item, card_w, card_h, true);

        let place = agents
            .iter()
            .map(|agent| agent.place.as_str())
            .find(|place| !place.is_empty())
            .unwrap_or_default()
            .to_string();
        let mut args = fluent_bundle::FluentArgs::new();
        args.set("count", agents.len());
        let count = crate::i18n::tr_args("hover-card-agents", &args);
        let count_w = self
            .cached_ui_text_advance(&style.body, &style.metrics, &count)?
            .ceil();

        let gl_state = self.render_state.as_ref().context("hover render state")?;
        let layer = gl_state
            .layer_for_zindex(crate::termwindow::TOOLTIP_ZINDEX)
            .context("thread card layer")?;
        let mut layers = layer.quad_allocator();
        self.paint_hover_surface(
            &mut layers,
            style,
            euclid::rect(x, y, card_w, card_h),
            self.ui_f32(CARD_RADIUS),
        )?;

        // The thread, and how many agents it holds.
        let mut cy = y + pad_y;
        self.paint_agent_text(
            &mut layers,
            &style.strong,
            style.strong_metrics,
            &place,
            (x + pad_x).round() as usize,
            cy.round() as usize,
            (card_w - pad_x * 2.0 - count_w - gap).max(0.0) as usize,
            style.palette.text,
        )?;
        self.paint_agent_text(
            &mut layers,
            &style.body,
            style.metrics,
            &count,
            (x + card_w - pad_x - count_w).round() as usize,
            cy.round() as usize,
            count_w as usize,
            style.palette.muted_text,
        )?;
        cy += header_h + gap;
        self.paint_hover_separator(&mut layers, x, cy, card_w, style)?;
        cy += 1.0 + gap;

        let right = x + text_x + text_w;
        let active_pane_id = self.get_active_pane_no_overlay().map(|pane| pane.pane_id());
        let pane_word = crate::i18n::tr("hover-card-pane");
        for agent in &agents[..rows] {
            let state = agent.shown_state();
            let brand = brand_of(agent);
            // Which pane, as the command line names it, at the end of the
            // title's line: the title gives way to it.
            let pane = format!("{pane_word} {}", cli_pane_id(agent.pane_id));
            let pane_w = self
                .cached_ui_text_advance(&style.body, &style.metrics, &pane)?
                .ceil()
                .min(text_w);
            self.paint_agent_text(
                &mut layers,
                &style.body,
                style.metrics,
                &pane,
                (right - pane_w).round() as usize,
                cy.round() as usize,
                pane_w as usize,
                style.palette.muted_text,
            )?;
            self.paint_agent_mark(
                &mut layers,
                agent,
                (x + pad_x).round() as usize,
                cy.round() as usize,
                mark.round() as usize,
                style.palette.secondary_text,
            )?;
            self.paint_hover_state_icon(
                &mut layers,
                state,
                Some(agent.pane_id) == active_pane_id,
                x + card_w - pad_x - mark,
                cy,
                mark,
                style,
            )?;
            self.paint_agent_text(
                &mut layers,
                &style.strong,
                style.strong_metrics,
                &agent_row_title(agent, brand),
                (x + text_x).round() as usize,
                cy.round() as usize,
                (text_w - pane_w - gap / 2.0).max(0.0) as usize,
                style.palette.text,
            )?;

            // The state in words, so it never rests on the icon's colour
            // alone, and how far it has got; then why it waits and what it
            // says.
            let said_y = (cy + line + spacing).round() as usize;
            let (kind, msg) = agent_status::report_said(agent, show);
            let mut status = crate::i18n::tr(agent_state_key(state));
            if let Some(progress) = agent_status::report_progress(agent, show) {
                status = format!("{status} \u{b7} {progress}%");
            }
            if kind.is_some() || msg.is_some() {
                status.push_str(" \u{b7} ");
            }
            let mut said_x = x + text_x;
            let status_w = self
                .cached_ui_text_advance(&style.body, &style.metrics, &status)?
                .ceil();
            self.paint_agent_text(
                &mut layers,
                &style.body,
                style.metrics,
                &status,
                said_x.round() as usize,
                said_y,
                status_w.min(right - said_x).max(0.0) as usize,
                style.palette.muted_text,
            )?;
            said_x += status_w;
            if let Some(kind) = kind {
                let label = crate::i18n::tr(agent_status::blocked_kind_key(kind));
                said_x = self.paint_agent_kind_pill(
                    &mut layers,
                    &style.body,
                    style.metrics,
                    &label,
                    said_x.round() as usize,
                    said_y,
                    right.max(0.0) as usize,
                )? as f32;
            }
            if let Some(msg) = msg {
                let color = said_color(state, style);
                self.paint_agent_text(
                    &mut layers,
                    &style.body,
                    style.metrics,
                    msg,
                    said_x.round() as usize,
                    said_y,
                    (right - said_x).max(0.0) as usize,
                    color,
                )?;
            }
            cy += entry_h + gap;
        }
        let more = agents.len() - rows;
        if more > 0 {
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("count", more);
            self.paint_agent_text(
                &mut layers,
                &style.body,
                style.metrics,
                &crate::i18n::tr_args("hover-card-more-agents", &args),
                (x + text_x).round() as usize,
                cy.round() as usize,
                text_w.max(0.0) as usize,
                style.palette.muted_text,
            )?;
        }
        Ok(())
    }

    /// An Agents panel row's card: everything the row has no room for. The
    /// title and the description in full (wrapped), the agent and its state,
    /// progress, then where it runs, on which machine, for how long it has
    /// been in this state, its sub-tasks and which pane it is. Left of the
    /// panel, which runs along the window's right edge.
    fn paint_agent_hover_card(
        &mut self,
        item: &UIItem,
        agent: &AgentPaneStatus,
        style: &HoverStyle,
    ) -> anyhow::Result<()> {
        let show = crate::native_settings::agent_status_display();
        let CardFrame {
            width: card_w,
            pad_x,
            pad_y,
            gap,
            spacing,
            line,
        } = self.card_frame(style);
        let mark = line;
        let inner_w = card_w - pad_x * 2.0;
        let shown = agent.shown_state();

        let title = agent_title(agent);
        let title_w = inner_w - mark - gap / 2.0;
        let wrap_title = |this: &Self, lines: usize| {
            this.wrap_hover_text(&style.strong, &style.strong_metrics, &title, title_w, lines)
        };
        let mut title_lines = wrap_title(self, CARD_TITLE_LINES)?;
        let subtitle = format!(
            "{} \u{b7} {}",
            agent_status::display_name(&agent.agent_id),
            crate::i18n::tr(agent_state_key(shown))
        );
        let (kind, msg) = agent_status::report_said(agent, show);
        let wrap_msg = |this: &Self, lines: usize| match msg {
            Some(msg) => this.wrap_hover_text(&style.body, &style.metrics, msg, inner_w, lines),
            None => Ok(Vec::new()),
        };
        let mut msg_lines = wrap_msg(self, CARD_MESSAGE_LINES)?;
        let progress = agent_status::report_progress(agent, show);
        let facts = self.agent_card_facts(agent, show);
        let mut label_w = 0.0f32;
        for (label, _) in &facts {
            label_w =
                label_w.max(self.cached_ui_text_advance(&style.body, &style.metrics, label)?);
        }
        let label_w = label_w.ceil();

        let lines_h = |count: usize| line * count as f32 + spacing * count.saturating_sub(1) as f32;
        let bar_h = self.ui_px(AGENT_PROGRESS_HEIGHT) as f32;
        let said = kind.is_some() || !msg_lines.is_empty() || progress.is_some();
        // Everything but the title's and the description's lines, which are
        // what gives way when the window is too short for the card: the
        // description down to one line, then the title, each fading where
        // it is cut.
        let fixed = pad_y * 2.0
            + spacing
            + line
            + if said {
                gap + 1.0
                    + gap
                    + if kind.is_some() { line + spacing } else { 0.0 }
                    + progress.map_or(0.0, |_| gap / 2.0 + bar_h)
            } else {
                0.0
            }
            + gap
            + 1.0
            + gap
            + lines_h(facts.len());
        let card_h_for = |title: usize, msg: usize| fixed + lines_h(title.max(1)) + lines_h(msg);
        let room = self.dimensions.pixel_height as f32 - gap * 2.0;
        let over = |height: f32| ((height - room) / (line + spacing)).ceil().max(0.0) as usize;
        let excess = over(card_h_for(title_lines.len(), msg_lines.len()));
        if excess > 0 && msg_lines.len() > 1 {
            msg_lines = wrap_msg(self, msg_lines.len().saturating_sub(excess).max(1))?;
        }
        if over(card_h_for(title_lines.len(), msg_lines.len())) > 0 && title_lines.len() > 1 {
            title_lines = wrap_title(self, 1)?;
        }
        let card_h = card_h_for(title_lines.len(), msg_lines.len());
        let (x, y) = self.hover_card_origin(item, card_w, card_h, false);

        let gl_state = self.render_state.as_ref().context("hover render state")?;
        let layer = gl_state
            .layer_for_zindex(crate::termwindow::TOOLTIP_ZINDEX)
            .context("agent card layer")?;
        let mut layers = layer.quad_allocator();
        self.paint_hover_surface(
            &mut layers,
            style,
            euclid::rect(x, y, card_w, card_h),
            self.ui_f32(CARD_RADIUS),
        )?;

        // Mark and title, then which agent and in what state.
        let left = x + pad_x;
        let mut cy = y + pad_y;
        self.paint_agent_mark(
            &mut layers,
            agent,
            left.round() as usize,
            cy.round() as usize,
            mark.round() as usize,
            style.palette.secondary_text,
        )?;
        for range in &title_lines {
            self.paint_agent_text(
                &mut layers,
                &style.strong,
                style.strong_metrics,
                &title[range.clone()],
                (left + mark + gap / 2.0).round() as usize,
                cy.round() as usize,
                title_w.max(0.0) as usize,
                style.palette.text,
            )?;
            cy += line + spacing;
        }
        if title_lines.is_empty() {
            cy += line + spacing;
        }
        self.paint_agent_text(
            &mut layers,
            &style.body,
            style.metrics,
            &subtitle,
            (left + mark + gap / 2.0).round() as usize,
            cy.round() as usize,
            title_w.max(0.0) as usize,
            style.palette.muted_text,
        )?;
        cy += line;

        // What it is doing or waits for, and how far it has got.
        if said {
            cy += gap;
            self.paint_hover_separator(&mut layers, x, cy, card_w, style)?;
            cy += 1.0 + gap;
            if let Some(kind) = kind {
                let label = crate::i18n::tr(agent_status::blocked_kind_key(kind));
                self.paint_agent_kind_pill(
                    &mut layers,
                    &style.body,
                    style.metrics,
                    &label,
                    left.round() as usize,
                    cy.round() as usize,
                    (left + inner_w).max(0.0) as usize,
                )?;
                cy += line + spacing;
            }
            if let Some(msg) = msg {
                let color = said_color(shown, style);
                for range in &msg_lines {
                    self.paint_agent_text(
                        &mut layers,
                        &style.body,
                        style.metrics,
                        &msg[range.clone()],
                        left.round() as usize,
                        cy.round() as usize,
                        inner_w.max(0.0) as usize,
                        color,
                    )?;
                    cy += line + spacing;
                }
                cy -= spacing;
            }
            if let Some(progress) = progress {
                cy += gap / 2.0;
                self.paint_agent_progress(
                    &mut layers,
                    euclid::rect(left, cy, inner_w, bar_h),
                    progress,
                    style.palette.separator,
                    style.palette.accent,
                )?;
                cy += bar_h;
            }
        }

        // Where, on which machine, for how long, its sub-tasks and its pane.
        cy += gap;
        self.paint_hover_separator(&mut layers, x, cy, card_w, style)?;
        cy += 1.0 + gap;
        let value_x = left + label_w + gap;
        for (label, value) in &facts {
            self.paint_agent_text(
                &mut layers,
                &style.body,
                style.metrics,
                label,
                left.round() as usize,
                cy.round() as usize,
                label_w as usize,
                style.palette.muted_text,
            )?;
            self.paint_agent_text(
                &mut layers,
                &style.body,
                style.metrics,
                value,
                value_x.round() as usize,
                cy.round() as usize,
                (left + inner_w - value_x).max(0.0) as usize,
                style.palette.secondary_text,
            )?;
            cy += line + spacing;
        }
        Ok(())
    }

    /// The labelled facts an agent card ends with. The machine's name reads
    /// the host list, so it is looked up the first time the card paints and
    /// kept with the hover; with no thread behind the agent's workspace there
    /// is no saying which machine, and the fact is left out. How long the
    /// agent has been in its state is painted again when that label next
    /// changes.
    fn agent_card_facts(
        &mut self,
        agent: &AgentPaneStatus,
        show: crate::native_settings::NativeAgentStatusDisplay,
    ) -> Vec<(String, String)> {
        let mut facts = Vec::with_capacity(5);
        if !agent.place.is_empty() {
            facts.push((crate::i18n::tr("hover-card-thread"), agent.place.clone()));
        }
        let kept = self
            .hover_tooltip
            .as_ref()
            .and_then(|hover| hover.machine.clone());
        let machine = match kept {
            Some(machine) => machine,
            None => {
                let machine = crate::workspace_threads::thread_machine_label(&agent.workspace)
                    .map(|label| label.unwrap_or_else(|| crate::i18n::tr("menu-machine-local")));
                if let Some(hover) = self.hover_tooltip.as_mut() {
                    hover.machine = Some(machine.clone());
                }
                machine
            }
        };
        if let Some(machine) = machine {
            facts.push((crate::i18n::tr("hover-card-machine"), machine));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if agent.since_unix > 0 {
            let elapsed = now.saturating_sub(agent.since_unix);
            facts.push((
                crate::i18n::tr("hover-card-duration"),
                elapsed_label(elapsed),
            ));
            self.update_next_frame_time(Some(
                Instant::now() + Duration::from_secs(label_changes_in(elapsed)),
            ));
        }
        if let Some(report) = agent.report.as_ref().filter(|_| show.subtasks) {
            if !report.children.is_empty() {
                let working = working_subtasks(report);
                let mut value = report.children.len().to_string();
                if working > 0 {
                    let mut args = fluent_bundle::FluentArgs::new();
                    args.set("count", working);
                    value = format!(
                        "{value} \u{b7} {}",
                        crate::i18n::tr_args("right-agents-count-working", &args)
                    );
                }
                facts.push((crate::i18n::tr("hover-card-subtasks"), value));
            }
        }
        let mut pane = cli_pane_id(agent.pane_id);
        if let Some(label) = mux::Mux::get().pane_label(agent.pane_id) {
            pane = format!("{pane} \u{b7} {label}");
        }
        facts.push((crate::i18n::tr("hover-card-pane"), pane));
        facts
    }
}

/// Pane `pane_id` as `thinkterm cli --pane` names it, so that what a card
/// says is what a program types: `MACHINE/ID` for one on a server, its
/// number on the session server for one that server keeps for this machine,
/// and its number here for any other.
fn cli_pane_id(pane_id: PaneId) -> String {
    if let Some(on) = wezterm_mux_server_impl::foreign_panes::on_its_machine(pane_id) {
        return format!("{}/{}", on.machine, on.pane_id);
    }
    mux::Mux::get()
        .get_pane(pane_id)
        .filter(|pane| crate::local_sessions::is_host_domain_id(pane.domain_id()))
        .and_then(|pane| {
            pane.downcast_ref::<wezterm_client::pane::ClientPane>()
                .map(|client| client.remote_pane_id)
        })
        .unwrap_or(pane_id)
        .to_string()
}

/// How long ago, in the largest whole unit that fits.
fn elapsed_label(seconds: u64) -> String {
    let (key, count) = match seconds {
        0..=59 => return crate::i18n::tr("hover-card-under-a-minute"),
        60..=3599 => ("hover-card-minutes", seconds / 60),
        3600..=86_399 => ("hover-card-hours", seconds / 3600),
        _ => ("hover-card-days", seconds / 86_400),
    };
    let mut args = fluent_bundle::FluentArgs::new();
    args.set("count", count);
    crate::i18n::tr_args(key, &args)
}

/// Seconds until [`elapsed_label`] reads differently.
fn label_changes_in(seconds: u64) -> u64 {
    let unit = match seconds {
        0..=3599 => 60,
        3600..=86_399 => 3600,
        _ => 86_400,
    };
    unit - seconds % unit
}

/// The sizes a card is laid out with: its width, its padding, the gap
/// between its parts, the spacing between wrapped lines and a line's height.
struct CardFrame {
    width: f32,
    pad_x: f32,
    pad_y: f32,
    gap: f32,
    spacing: f32,
    line: f32,
}

/// What an agent says is set in the card's grey, or red when it failed.
fn said_color(state: ShownState, style: &HoverStyle) -> LinearRgba {
    if state == ShownState::Error {
        style.palette.danger
    } else {
        style.palette.secondary_text
    }
}

/// Punctuation that closes, which no line may start with.
fn closes(c: char) -> bool {
    "，。、：；！？）」』】》〉〕｝〗〙・…ー々ゝゞヽヾ”’,.:;!?)]}".contains(c)
}

/// Punctuation that opens, which no line may end with.
fn opens(c: char) -> bool {
    "（「『【《〈〔｛〖〘“‘([{".contains(c)
}

/// The line breaks for glyphs -- (cluster byte offset, advance) in order --
/// of `text`, as `TermWindow::wrap_hover_text` describes them.
fn wrap_glyphs(
    text: &str,
    glyphs: &[(usize, f32)],
    width: f32,
    max_lines: usize,
) -> Vec<Range<usize>> {
    let char_at = |cluster: usize| text.get(cluster..).and_then(|rest| rest.chars().next());
    let mut lines = Vec::new();
    let mut start = 0usize;
    while lines.len() + 1 < max_lines && start < glyphs.len() {
        let origin = glyphs[start].0;
        let mut x = 0.0f32;
        let mut over = None;
        for (i, &(_, advance)) in glyphs.iter().enumerate().skip(start) {
            x += advance;
            if x > width && i > start {
                over = Some(i);
                break;
            }
        }
        let Some(over) = over else {
            break;
        };
        // The latest place to break before `over`: before a space, after a
        // slash, or between two characters of a script set without spaces --
        // but never before closing punctuation or after opening, so no line
        // starts with "，" or ends with "（".
        let mut cut = None;
        for i in (start + 1..=over).rev() {
            let here = char_at(glyphs[i].0);
            let before = char_at(glyphs[i - 1].0);
            let wide = |c: Option<char>| c.is_some_and(|c| c as u32 >= 0x2E80);
            let breaks = here.is_some_and(char::is_whitespace)
                || before == Some('/')
                || ((wide(here) || wide(before))
                    && !here.is_some_and(closes)
                    && !before.is_some_and(opens));
            if breaks {
                cut = Some(i);
                break;
            }
        }
        let cut = cut.unwrap_or(over);
        let end = glyphs[cut].0;
        let line = text[origin..end].trim_end();
        lines.push(origin..origin + line.len());
        // The next line starts past the spaces at the break.
        start = cut;
        while start < glyphs.len() && char_at(glyphs[start].0).is_some_and(char::is_whitespace) {
            start += 1;
        }
    }
    if start < glyphs.len() {
        let origin = glyphs[start].0;
        let rest = text[origin..].trim_end();
        lines.push(origin..origin + rest.len());
    }
    lines
}

/// The agent an Agents panel row reveals: the row's own pane.
fn agent_row_pane(item_type: &UIItemType) -> Option<PaneId> {
    match item_type {
        UIItemType::RightSidebarAgent(
            AgentPanelAction::Reveal(pane_id) | AgentPanelAction::RevealElsewhere(pane_id),
        ) => Some(*pane_id),
        _ => None,
    }
}

/// The agent whose card an item shows: an Agents panel row's, over the row
/// and over its sub-task line, a target of its own on top of the row.
pub(crate) fn agent_card_pane(item_type: &UIItemType) -> Option<PaneId> {
    match item_type {
        UIItemType::RightSidebarAgent(AgentPanelAction::ToggleSubtasks(pane_id)) => Some(*pane_id),
        _ => agent_row_pane(item_type),
    }
}

/// Whether the pointer moving from `a` to `b` stays on one surface: the same
/// item, or two parts of the same agent's row.
pub(crate) fn same_hover_target(a: &UIItemType, b: &UIItemType) -> bool {
    a == b || agent_card_pane(a).is_some_and(|pane_id| agent_card_pane(b) == Some(pane_id))
}

#[cfg(test)]
mod tests {
    use super::{label_changes_in, wrap_glyphs};

    /// One unit of advance per character, the cluster its byte offset.
    fn lines(text: &str, width: f32, max: usize) -> Vec<&str> {
        let glyphs: Vec<(usize, f32)> = text.char_indices().map(|(at, _)| (at, 1.0)).collect();
        wrap_glyphs(text, &glyphs, width, max)
            .into_iter()
            .map(|range| &text[range])
            .collect()
    }

    #[test]
    fn short_text_is_one_line() {
        assert_eq!(lines("Find Thread", 20.0, 3), ["Find Thread"]);
    }

    #[test]
    fn words_wrap_before_the_one_that_does_not_fit() {
        assert_eq!(
            lines("alpha beta gamma delta", 11.0, 3),
            ["alpha beta", "gamma delta"]
        );
    }

    #[test]
    fn paths_wrap_after_a_slash() {
        assert_eq!(
            lines("/home/user/project/src/main.rs", 12.0, 3),
            ["/home/user/", "project/src/", "main.rs"]
        );
    }

    #[test]
    fn the_time_in_a_state_is_redrawn_when_its_label_changes() {
        assert_eq!(label_changes_in(0), 60);
        assert_eq!(label_changes_in(59), 1);
        assert_eq!(label_changes_in(61), 59);
        assert_eq!(label_changes_in(3599), 1);
        assert_eq!(label_changes_in(3600), 3600);
        assert_eq!(label_changes_in(86_399), 1);
        assert_eq!(label_changes_in(86_400), 86_400);
    }

    #[test]
    fn the_last_line_keeps_the_rest() {
        assert_eq!(
            lines("one two three four five six", 8.0, 2),
            ["one two", "three four five six"]
        );
    }

    #[test]
    fn no_line_starts_with_closing_punctuation() {
        assert_eq!(
            lines("终端软件，推荐比较", 4.0, 3),
            ["终端软", "件，推荐", "比较"]
        );
        assert_eq!(lines("终端软件（推荐）", 5.0, 3), ["终端软件", "（推荐）"]);
        assert_eq!(lines("运行“测试”完成", 3.0, 3), ["运行", "“测", "试”完成"]);
    }

    #[test]
    fn text_without_spaces_breaks_anywhere() {
        assert_eq!(
            lines("终端软件推荐与比较", 4.0, 3),
            ["终端软件", "推荐与比", "较"]
        );
        assert_eq!(lines("abcdefghij", 4.0, 2), ["abcd", "efghij"]);
    }
}
