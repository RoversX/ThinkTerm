use crate::model::AppModel;
use crate::settings::TuiConfig;
use crate::state::{AppMode, ConnectionStatus, SelectionPoint, TextSelection, UiState, ViewClass};
use crate::view::{PaneNav, PaneView, SplitView, ViewLayout};
use mux::pane::{Pane, PaneId, SearchResult};
use mux::tab::{SplitDirection, Tab, TabId};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use std::sync::Arc;
use termwiz::cell::{unicode_column_width, Blink, CellAttributes, Intensity, Underline};
use termwiz::color::ColorAttribute;
use wezterm_term::color::SrgbaTuple;
use wezterm_term::StableRowIndex;

#[derive(Clone, Debug, Default)]
pub struct RenderResult {
    /// This ID is in the remote server namespace.
    pub selected_tab: Option<TabId>,
    pub active_pane: Option<PaneId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandoffAnimationFrame {
    eyes_closed: bool,
}

impl Default for HandoffAnimationFrame {
    fn default() -> Self {
        Self { eyes_closed: false }
    }
}

impl HandoffAnimationFrame {
    pub fn at(elapsed: std::time::Duration) -> Self {
        let millis = elapsed.as_millis() as u64;
        Self {
            eyes_closed: millis % 5_000 >= 4_750,
        }
    }
}

/// Rendering is intentionally read-only. Selection normalization and geometry
/// are completed before this function is called, so a draw can never change
/// what the next mouse event means.
pub fn render(
    frame: &mut Frame<'_>,
    model: &AppModel,
    ui: &UiState,
    local_tab: Option<&Arc<Tab>>,
    view: &ViewLayout,
    viewport_status: Option<&str>,
    handoff_message: Option<&(String, String)>,
    handoff_animation: HandoffAnimationFrame,
    shared_grid: bool,
    settings: &TuiConfig,
) -> RenderResult {
    let mut result = RenderResult {
        selected_tab: model.selected_tab().map(|tab| tab.tab_id),
        ..Default::default()
    };
    frame.render_widget(Clear, view.screen);
    if view.screen.width == 0 || view.screen.height == 0 {
        return result;
    }
    if view.screen.width < 20 || view.screen.height < 4 {
        render_center(frame, view.screen, "Terminal too small", chrome_selected());
        return result;
    }

    render_sidebar(frame, view);
    render_tabs(frame, view);
    render_status(frame, model, ui, view, viewport_status);

    if let Some((title, hint)) = handoff_message {
        render_handoff(frame, view.content, title, hint, handoff_animation);
    } else if let Some(tab) = local_tab {
        result.active_pane = tab.get_active_pane().map(|pane| pane.pane_id());
        if shared_grid {
            render_shared_grid(frame, view.content);
        }
        render_panes(frame, tab, ui, view);
    } else {
        let message = match model.selected_row() {
            None => "Opening your local terminal…",
            Some(row) if row.thread.tabs.is_empty() => "Opening terminal…",
            Some(_) => "Waiting for the mux view",
        };
        render_center(frame, view.content, message, chrome_dim());
    }

    render_overlays(frame, ui, view, settings);
    result
}

fn render_handoff(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    hint: &str,
    animation: HandoffAnimationFrame,
) {
    frame.render_widget(Clear, area);
    fill(frame, area, " ", chrome());
    if area.width == 0 || area.height == 0 {
        return;
    }
    let show_eyes = area.width >= 12 && area.height >= 5;
    let group_height = if show_eyes { 4 } else { 2 };
    let start_y = area.y + area.height.saturating_sub(group_height) / 2;
    if show_eyes {
        let eyes = if animation.eyes_closed {
            "─  ─"
        } else {
            "•  •"
        };
        render_center(
            frame,
            Rect::new(area.x, start_y, area.width, 1),
            eyes,
            chrome_accent_text(),
        );
    }
    let title_y = start_y + if show_eyes { 2 } else { 0 };
    render_center(
        frame,
        Rect::new(area.x, title_y, area.width, 1),
        title,
        chrome_bold(),
    );
    if title_y + 1 < area.bottom() {
        render_center(
            frame,
            Rect::new(area.x, title_y + 1, area.width, 1),
            hint,
            chrome_dim(),
        );
    }
}

fn render_shared_grid(frame: &mut Frame<'_>, area: Rect) {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if (x + y) % 2 == 0 {
                frame.buffer_mut()[(x, y)]
                    .set_symbol("·")
                    .set_style(chrome_dim());
            }
        }
    }
}

fn render_sidebar(frame: &mut Frame<'_>, view: &ViewLayout) {
    let Some(sidebar) = view.sidebar else {
        return;
    };
    fill(frame, sidebar, " ", chrome());
    if let Some(header) = view.sidebar_header {
        let title = match view.class {
            ViewClass::Compact => " ≡ TT",
            _ => " ≡ THINKTERM",
        };
        draw_text(
            frame,
            header,
            title,
            chrome_accent_text().add_modifier(Modifier::BOLD),
        );
    }
    if let Some(rect) = view.sidebar_new_thread {
        // Text in the accent colour, not a second filled bar: the header above
        // is already solid, and two stacked blocks of colour read as one heavy
        // slab rather than as a title and a button.
        draw_text(frame, rect, " ⊕ New Thread", chrome_accent_text());
    }
    if let Some(rect) = view.sidebar_heading {
        draw_text(frame, rect, " WORKSPACES", chrome_dim());
    }
    if let Some(rect) = view.sidebar_new_project {
        draw_text(frame, rect, "+", chrome_bold());
    }
    for line in &view.tree_lines {
        let mut label = String::new();
        label.push_str(&"  ".repeat(line.depth as usize));
        if line.expandable {
            label.push_str(if line.collapsed { "▸ " } else { "▾ " });
        } else {
            label.push_str("  ");
        }
        if let Some(status) = line.status {
            label.push_str(status);
            label.push(' ');
        }
        if line.pinned {
            label.push_str("★ ");
        }
        label.push_str(&line.label);
        // The selected row is lifted by a surface block and bold text, not by a
        // slab of accent: at three nesting levels a saturated bar is the
        // loudest thing on the screen, and it is only saying "here".
        let style = match (line.selected, line.status) {
            (true, _) => chrome_surface().add_modifier(Modifier::BOLD),
            (false, Some("!")) => chrome_danger().add_modifier(Modifier::BOLD),
            (false, Some("+")) => chrome_success().add_modifier(Modifier::BOLD),
            // Running had no colour of its own, so the one status that changes
            // by itself was the one the eye could not find.
            (false, Some("*")) => chrome_accent_text().add_modifier(Modifier::BOLD),
            (false, _) if line.depth < 3 => chrome_bold(),
            _ => chrome_dim(),
        };
        fill(frame, line.rect, " ", style);
        // The label is clipped to where the buttons begin, so a long name is
        // shortened rather than drawn underneath them.
        let label_width = line
            .actions
            .iter()
            .map(|(rect, _)| rect.x)
            .min()
            .unwrap_or(line.rect.right())
            .saturating_sub(line.rect.x);
        draw_text(
            frame,
            Rect::new(line.rect.x, line.rect.y, label_width, line.rect.height),
            &label,
            style,
        );
        // On the row being worked on the buttons share its weight; elsewhere
        // they stay out of the way of the names, which is what the list is for.
        let action_style = if line.selected {
            style
        } else {
            style.patch(chrome_dim())
        };
        for (rect, action) in &line.actions {
            draw_text(frame, *rect, action.glyph(), action_style);
        }
    }
    if let Some(rect) = view.sidebar_settings {
        fill(frame, rect, " ", chrome_dim());
        draw_text(frame, rect, " ⚙ Settings", chrome_dim());
    }
    if let Some(resize) = view.sidebar_resize {
        for y in resize.y..resize.bottom() {
            frame.buffer_mut()[(resize.x, y)]
                .set_symbol("│")
                .set_style(chrome_border());
        }
    }
}

fn render_tabs(frame: &mut Frame<'_>, view: &ViewLayout) {
    if view.tab_bar.is_empty() {
        return;
    }
    fill(frame, view.tab_bar, " ", chrome());
    if view.sidebar.is_none() {
        if let Some(toggle) = view.sidebar_toggle {
            draw_text(
                frame,
                toggle,
                " ≡ ",
                chrome_accent_text().add_modifier(Modifier::BOLD),
            );
        }
    }
    for (rect, control) in &view.tab_controls {
        draw_text(frame, *rect, control.glyph(), chrome_dim());
    }
    if view.tabs.is_empty() {
        draw_text(frame, view.tab_bar, " Opening terminal…", chrome_dim());
        return;
    }
    for tab in &view.tabs {
        // The active tab is the single filled block on the screen, which is
        // what makes it findable at a glance.
        let style = if tab.selected {
            chrome_selected()
        } else {
            chrome_dim()
        };
        fill(frame, tab.rect, " ", style);
        draw_text(frame, tab.rect, &tab.label, style);
        if let Some(close) = tab.close {
            draw_text(frame, close, "✕", style);
        }
    }
    let used = view
        .tabs
        .last()
        .map_or(view.tab_bar.x, |tab| tab.rect.right());
    if used < view.tab_bar.right() {
        let separator = Rect::new(used, view.tab_bar.y, 1, view.tab_bar.height);
        fill(frame, separator, "│", chrome_border());
    }
}

fn render_panes(frame: &mut Frame<'_>, tab: &Arc<Tab>, ui: &UiState, view: &ViewLayout) {
    let panes = tab.iter_panes();
    for pane_view in &view.panes {
        let Some(positioned) = panes
            .iter()
            .find(|positioned| positioned.pane.pane_id() == pane_view.pane_id)
        else {
            continue;
        };
        if let Some(collapsed) = pane_view.collapsed {
            paint_collapsed_pane(
                frame,
                &positioned.pane,
                pane_view,
                collapsed,
                tab.get_active_pane()
                    .is_some_and(|active| active.pane_id() == pane_view.pane_id),
            );
            continue;
        }
        if let Some(border) = pane_view.border {
            let focused = tab
                .get_active_pane()
                .is_some_and(|active| active.pane_id() == pane_view.pane_id);
            let style = if focused {
                chrome_accent_text()
            } else {
                chrome_border()
            };
            frame.render_widget(
                Block::default().borders(Borders::ALL).border_style(style),
                border,
            );
        }
        if let Some(nav) = &pane_view.nav {
            let focused = tab
                .get_active_pane()
                .is_some_and(|active| active.pane_id() == pane_view.pane_id);
            paint_pane_nav(frame, nav, focused);
        }
        paint_pane(frame, &positioned.pane, pane_view.rect, ui);
        if let Some(bar) = pane_view.scrollbar {
            paint_scrollbar(frame, &positioned.pane, bar, ui);
        }
    }

    // Framed panes already show where each one ends, and a divider between two
    // frames is a third line of chrome in a row that has two. When the frames
    // were not drawn — too small, or turned off — the divider is the only thing
    // separating them.
    for split in &view.splits {
        let symbol = match split.direction {
            SplitDirection::Horizontal => "│",
            SplitDirection::Vertical => "─",
        };
        for y in split.rect.y..split.rect.bottom() {
            for x in split.rect.x..split.rect.right() {
                if !split_cell_needs_divider(view, split, x, y) {
                    continue;
                }
                frame.buffer_mut()[(x, y)]
                    .set_symbol(symbol)
                    .set_style(chrome_border());
            }
        }
    }

    if let Some(active) = tab.get_active_pane() {
        if let Some(pane_view) = view
            .panes
            .iter()
            .find(|pane| pane.pane_id == active.pane_id() && pane.collapsed.is_none())
        {
            let dims = active.get_dimensions();
            let offset = ui.scroll_offset(active.pane_id());
            if offset == 0 {
                let cursor = active.get_cursor_position();
                let visible_y = cursor.y - dims.physical_top;
                if visible_y >= 0
                    && visible_y < pane_view.rect.height as isize
                    && cursor.x < pane_view.rect.width as usize
                {
                    frame.set_cursor_position((
                        pane_view.rect.x + cursor.x as u16,
                        pane_view.rect.y + visible_y as u16,
                    ));
                }
            }
        }
    }
}

fn split_cell_needs_divider(view: &ViewLayout, split: &SplitView, x: u16, y: u16) -> bool {
    let framed_on_both_sides = match split.direction {
        SplitDirection::Horizontal => {
            let left = view.panes.iter().any(|pane| {
                pane.border.is_some_and(|border| {
                    border.right() == x && y >= border.y && y < border.bottom()
                })
            });
            let right = view.panes.iter().any(|pane| {
                pane.border.is_some_and(|border| {
                    border.x == x.saturating_add(1) && y >= border.y && y < border.bottom()
                })
            });
            left && right
        }
        SplitDirection::Vertical => {
            let above = view.panes.iter().any(|pane| {
                pane.border.is_some_and(|border| {
                    border.bottom() == y && x >= border.x && x < border.right()
                })
            });
            let below = view.panes.iter().any(|pane| {
                pane.border.is_some_and(|border| {
                    border.y == y.saturating_add(1) && x >= border.x && x < border.right()
                })
            });
            above && below
        }
    };
    !framed_on_both_sides
}

fn paint_collapsed_pane(
    frame: &mut Frame<'_>,
    pane: &Arc<dyn Pane>,
    pane_view: &PaneView,
    area: Rect,
    focused: bool,
) {
    fill(frame, area, " ", chrome());
    if let Some(nav) = &pane_view.nav {
        paint_pane_nav(frame, nav, focused);
        return;
    }
    let title = pane.get_title();
    let title = if title.trim().is_empty() {
        "shell"
    } else {
        title.trim()
    };
    let label = format!(" … {title} (pane too small) ");
    draw_text(
        frame,
        area,
        &label,
        if focused {
            chrome_accent_text().add_modifier(Modifier::BOLD)
        } else {
            chrome_dim()
        },
    );
}

/// Draw one pane's own strip: what is stacked behind it, and what can be done
/// to it.
///
/// The focused pane's bar is the only one drawn in the accent, which is what
/// says where typing goes — a job the frame used to do alone, and only when the
/// pane was wide enough to afford one.
fn paint_pane_nav(frame: &mut Frame<'_>, nav: &PaneNav, focused: bool) {
    if nav.rect.is_empty() {
        return;
    }
    // No fill. A band of surface behind every pane turned the screen into
    // stacked grey slabs — and because every style below sets its own
    // background, the glyphs punched the plain background back through it in
    // streaks. The bar sits on the background like everything else; which pane
    // has the focus is said by the colour of its name, which is where the eye
    // already is.
    let base = chrome();
    // No fill. On a framed pane this row *is* the frame's top edge, and filling
    // it with spaces rubs the line out between the corners, leaving `┌` and `┐`
    // floating either side of a gap. Leaving it alone lets the line run behind
    // the names and buttons, which is what a titled box looks like.
    for entry in &nav.tabs {
        // Each item clears its own cells. Filling the whole row would rub out
        // the frame's top edge; filling none of it lets the line run through
        // the gaps inside a name and between the buttons.
        fill(frame, entry.rect, " ", base);
        let style = match (entry.active, focused) {
            (true, true) => chrome_accent_text().add_modifier(Modifier::BOLD),
            (true, false) => chrome_bold(),
            (false, _) => chrome_dim(),
        };
        // Clipped to where the close button starts, so a long title is
        // shortened rather than drawn underneath it.
        // One column short of the close button, so a title long enough to be
        // clipped does not end up flush against the ✕ and read as part of it.
        let label = Rect::new(
            entry.rect.x,
            entry.rect.y,
            entry.close.map_or(entry.rect.width, |close| {
                (close.x - entry.rect.x).saturating_sub(1)
            }),
            entry.rect.height,
        );
        draw_text(frame, label, &entry.label, style);
        if let Some(close) = entry.close {
            draw_text(frame, close, "✕", style);
        }
    }
    let tool_style = if focused { chrome_bold() } else { chrome_dim() };
    for (rect, tool) in &nav.tools {
        fill(frame, *rect, " ", base);
        draw_text(frame, *rect, tool.glyph(), tool_style);
    }
}

/// Draw a pane's position in its own scrollback.
///
/// The thumb is sized by how much of the scrollback is on screen and placed by
/// how far back the view has been pulled, so its length says how much there is
/// and its position says where you are. A pane with nothing to scroll gets a
/// plain track rather than a full-height thumb, which would otherwise read as
/// "there is more here" on a screen that has none.
fn paint_scrollbar(frame: &mut Frame<'_>, pane: &Arc<dyn Pane>, area: Rect, ui: &UiState) {
    if area.is_empty() {
        return;
    }
    fill(frame, area, "│", chrome_border());
    let dims = pane.get_dimensions();
    let scrollable = dims.scrollback_rows.saturating_sub(dims.viewport_rows);
    if scrollable == 0 {
        return;
    }
    let height = area.height as usize;
    let thumb = ((dims.viewport_rows * height) / dims.scrollback_rows.max(1)).clamp(1, height);
    let offset = ui.scroll_offset(pane.pane_id()).min(scrollable);
    // Offset counts backwards from the live screen, and the bar runs forwards.
    let travel = height - thumb;
    let from_top = travel - (offset * travel) / scrollable.max(1);
    fill(
        frame,
        Rect::new(
            area.x,
            area.y + from_top as u16,
            area.width,
            thumb.min(height) as u16,
        ),
        "█",
        chrome_accent_text(),
    );
}

fn paint_pane(frame: &mut Frame<'_>, pane: &Arc<dyn Pane>, area: Rect, ui: &UiState) {
    if area.is_empty() {
        return;
    }
    // Note that this does *not* ask the pane what changed. Asking is what makes
    // a remote pane ask the server, and the server's answer notifies the mux,
    // which marks the frame dirty, which draws, which would ask again: an idle
    // screen measured at half a core and 19 KB/s to the tty, spent on nothing.
    // The asking lives on a timer in the event loop (`poll_panes`) so that its
    // rate is set by the clock rather than by how fast this machine can paint.
    let dims = pane.get_dimensions();
    let height = (area.height as usize).min(dims.scrollback_rows.max(dims.viewport_rows));
    let offset = ui.scroll_offset(pane.pane_id());
    let earliest = dims.scrollback_top;
    let top = dims
        .physical_top
        .saturating_sub(offset.min(dims.scrollback_rows) as StableRowIndex)
        .max(earliest);
    let (resolved_top, lines) = pane.get_lines(top..top + height as StableRowIndex);
    // Named and indexed colors are forwarded as themselves rather than looked
    // up in the pane's palette, so the host terminal resolves them the way it
    // resolves colors for every other program running in it. Resolving here
    // instead would mean a program's `ESC[31m` could arrive as some other
    // color entirely whenever the pane's palette is not what we assumed —
    // which is exactly how red logos ended up painted in the default
    // foreground. Only true color carries an absolute value, so only true
    // color keeps one.
    let blank_style = Style::reset();
    let buffer = frame.buffer_mut();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buffer[(x, y)].set_symbol(" ").set_style(blank_style);
        }
    }
    for (row, line) in lines.into_iter().take(area.height as usize).enumerate() {
        let y = area.y + row as u16;
        for cell in line.visible_cells() {
            if cell.cell_index() >= area.width as usize {
                break;
            }
            let x = area.x + cell.cell_index() as u16;
            let remaining = area.right().saturating_sub(x) as usize;
            buffer.set_stringn(x, y, cell.str(), remaining, cell_style(cell.attrs()));
        }
    }

    if let Some(selection) = ui
        .selection
        .as_ref()
        .filter(|selection| selection.pane_id == pane.pane_id())
    {
        render_selection(frame, area, resolved_top, selection, Modifier::REVERSED);
    }
    if let Some(copy) = ui
        .copy
        .as_ref()
        .filter(|copy| copy.pane_id == pane.pane_id())
    {
        if let Some(selection) = &copy.selection {
            render_selection(frame, area, resolved_top, selection, Modifier::REVERSED);
        }
        render_search_matches(
            frame,
            area,
            resolved_top,
            &copy.search.matches,
            copy.search.current,
        );
        if copy.cursor.row >= resolved_top
            && copy.cursor.row < resolved_top + area.height as StableRowIndex
            && copy.cursor.col < area.width as usize
        {
            let x = area.x + copy.cursor.col as u16;
            let y = area.y + (copy.cursor.row - resolved_top) as u16;
            let cell = &mut frame.buffer_mut()[(x, y)];
            cell.set_style(
                cell.style()
                    .add_modifier(Modifier::REVERSED | Modifier::BOLD),
            );
        }
    }
}

fn render_selection(
    frame: &mut Frame<'_>,
    area: Rect,
    visible_top: StableRowIndex,
    selection: &TextSelection,
    modifier: Modifier,
) {
    let (start, end) = selection.ordered();
    let visible_end = visible_top + area.height as StableRowIndex;
    for row in start.row.max(visible_top)..=(end.row.min(visible_end.saturating_sub(1))) {
        let first = if row == start.row { start.col } else { 0 };
        let last = if row == end.row {
            end.col.saturating_add(1)
        } else {
            area.width as usize
        };
        for col in first.min(area.width as usize)..last.min(area.width as usize) {
            let cell =
                &mut frame.buffer_mut()[(area.x + col as u16, area.y + (row - visible_top) as u16)];
            cell.set_style(cell.style().add_modifier(modifier));
        }
    }
}

fn render_search_matches(
    frame: &mut Frame<'_>,
    area: Rect,
    visible_top: StableRowIndex,
    matches: &[SearchResult],
    current: Option<usize>,
) {
    for (index, found) in matches.iter().enumerate() {
        let selection = TextSelection {
            pane_id: 0,
            anchor: SelectionPoint {
                row: found.start_y,
                col: found.start_x,
            },
            head: SelectionPoint {
                row: found.end_y,
                col: found.end_x,
            },
            finalized: true,
        };
        render_selection(
            frame,
            area,
            visible_top,
            &selection,
            if current == Some(index) {
                Modifier::REVERSED | Modifier::BOLD
            } else {
                Modifier::UNDERLINED
            },
        );
    }
}

fn render_status(
    frame: &mut Frame<'_>,
    model: &AppModel,
    ui: &UiState,
    view: &ViewLayout,
    viewport_status: Option<&str>,
) {
    if view.status.is_empty() {
        return;
    }
    // A full-width block of accent across the bottom of the screen is the
    // heaviest thing on it, and it is spending that weight on a hint. The bar
    // keeps the background; only a mode worth noticing gets picked out.
    fill(frame, view.status, " ", chrome());
    let left = match ui.mode {
        AppMode::Prefix => " PREFIX  ? help  b sidebar  g go  c tab  v/- split  x close ",
        AppMode::Navigate => " NAVIGATE  ↑↓ thread  hjkl pane  Enter open  Esc back ",
        AppMode::Resize => " RESIZE  hjkl/arrows resize  Enter/Esc finish ",
        AppMode::Copy => " COPY  hjkl move  Space select  / search  y copy  Esc back ",
        AppMode::Search => " SEARCH  Enter apply  Esc cancel ",
        AppMode::Connections => " CONNECTIONS  ↑↓ select  Enter connect  Esc back ",
        AppMode::Settings => " SETTINGS  ↑↓ select  ←→ change  Esc close ",
        AppMode::Help => " HELP  Esc close ",
        _ => " Ctrl-b commands ",
    };
    let mode_style = if ui.mode == AppMode::Terminal {
        chrome_dim()
    } else {
        chrome_accent_text().add_modifier(Modifier::BOLD)
    };
    draw_text(frame, view.status, left, mode_style);

    let message = if let Some(pending) = &ui.pending {
        format!(" {} on {}… ", pending.label, pending.domain_name)
    } else if !ui.status.is_empty() {
        format!(" {} ", ui.status)
    } else {
        format!(" r{} · #{} ", model.tree_revision(), model.generation())
    };
    let base_right = if model.attention_count() > 0 { 18 } else { 8 };
    let access_marker = viewport_status.map(|status| format!(" {status} "));
    let access_width = access_marker
        .as_deref()
        .map(|marker| unicode_column_width(marker, None).min(28) as u16)
        .unwrap_or(0)
        .min(view.status.width.saturating_sub(base_right));
    let reserve_right = base_right + access_width;
    let message_width = unicode_column_width(&message, None)
        .min(view.status.width.saturating_sub(reserve_right) as usize)
        as u16;
    if message_width > 0 {
        let rect = Rect::new(
            view.status
                .right()
                .saturating_sub(reserve_right + message_width),
            view.status.y,
            message_width,
            1,
        );
        draw_text(frame, rect, &message, chrome_dim());
    }

    if let (Some(marker), width) = (access_marker.as_deref(), access_width) {
        if width > 0 {
            let rect = Rect::new(
                view.status.right().saturating_sub(base_right + width),
                view.status.y,
                width,
                1,
            );
            draw_text(
                frame,
                rect,
                marker,
                chrome_accent_text().add_modifier(Modifier::BOLD),
            );
        }
    }

    if model.attention_count() > 0 && view.status.width >= 18 {
        let text = format!(" ! {} ", model.attention_count());
        let rect = Rect::new(view.status.right() - 17, view.status.y, 9, 1);
        draw_text(
            frame,
            rect,
            &text,
            chrome_danger().add_modifier(Modifier::BOLD),
        );
    }
    if view.status.width >= 8 {
        let rect = Rect::new(view.status.right() - 7, view.status.y, 7, 1);
        draw_text(frame, rect, " Detach", chrome_dim());
    }
}

fn render_overlays(frame: &mut Frame<'_>, ui: &UiState, view: &ViewLayout, settings: &TuiConfig) {
    if ui.mode == AppMode::Help {
        render_help(frame, view.screen);
    }
    if ui.mode == AppMode::Connections {
        render_connections(frame, ui, view);
    }
    if ui.mode == AppMode::Settings {
        render_settings(frame, ui, view, settings);
    }
    if let (Some(menu), Some(rect)) = (&ui.context_menu, view.context_menu) {
        frame.render_widget(Clear, rect);
        fill(frame, rect, " ", chrome());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(chrome_border())
            .style(chrome())
            .title(" Actions ");
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        for (index, entry) in menu.entries.iter().enumerate() {
            if index as u16 >= inner.height {
                break;
            }
            let row = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
            let style = if index == menu.selected {
                chrome_selected()
            } else if !entry.enabled {
                chrome_dim()
            } else if entry.destructive {
                chrome_danger().add_modifier(Modifier::BOLD)
            } else {
                chrome()
            };
            fill(frame, row, " ", style);
            draw_text(frame, row, &format!(" {}", entry.label), style);
        }
    }

    if let (Some(prompt), Some(rect)) = (&ui.prompt, view.dialog) {
        frame.render_widget(Clear, rect);
        fill(frame, rect, " ", chrome());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(chrome_border())
            .style(chrome())
            .title(format!(" {} ", prompt.title));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.height > 0 {
            let input = Rect::new(inner.x, inner.y + 1.min(inner.height - 1), inner.width, 1);
            fill(frame, input, " ", chrome_surface());
            draw_text(
                frame,
                input,
                &format!(" {}", prompt.value),
                chrome_surface(),
            );
            let cursor_x = input.x
                + 1
                + unicode_column_width(&prompt.value, None)
                    .min(input.width.saturating_sub(2) as usize) as u16;
            frame.set_cursor_position((cursor_x.min(input.right().saturating_sub(1)), input.y));
        }
        if inner.height > 2 {
            let hint = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
            draw_text(frame, hint, " Enter confirm · Esc cancel", chrome_dim());
        }
    }

    if let (Some(confirm), Some(rect)) = (&ui.confirmation, view.dialog) {
        frame.render_widget(Clear, rect);
        fill(frame, rect, " ", chrome());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(chrome_border())
            .style(chrome())
            .title(format!(" {} ", confirm.title));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let detail = Paragraph::new(confirm.detail.as_str())
            .style(chrome())
            .wrap(ratatui::widgets::Wrap { trim: true });
        let detail_rect = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(2),
        );
        frame.render_widget(detail, detail_rect);
        if inner.height >= 2 {
            let button_y = inner.bottom() - 1;
            let half = inner.width / 2;
            let confirm_rect = Rect::new(inner.x, button_y, half, 1);
            let cancel_rect = Rect::new(inner.x + half, button_y, inner.width - half, 1);
            fill(frame, confirm_rect, " ", chrome_bold_selected());
            draw_text(frame, confirm_rect, " Confirm", chrome_bold_selected());
            fill(frame, cancel_rect, " ", chrome_surface());
            draw_text(frame, cancel_rect, " Cancel", chrome_surface());
        }
    }

    if let Some(toast) = &ui.toast {
        let width = (unicode_column_width(&toast.message, None) as u16 + 4)
            .min(view.screen.width)
            .max(4);
        let rect = Rect::new(
            view.screen.right().saturating_sub(width),
            view.screen.y,
            width,
            3.min(view.screen.height),
        );
        frame.render_widget(Clear, rect);
        fill(frame, rect, " ", chrome());
        frame.render_widget(
            Paragraph::new(TextLine::from(vec![
                Span::styled(" + ", chrome_success().add_modifier(Modifier::BOLD)),
                Span::raw(toast.message.as_str()),
            ]))
            .style(chrome())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(chrome_border())
                    .style(chrome()),
            ),
            rect,
        );
    }
}

fn render_settings(frame: &mut Frame<'_>, ui: &UiState, view: &ViewLayout, settings: &TuiConfig) {
    let Some(rect) = view.dialog else {
        return;
    };
    frame.render_widget(Clear, rect);
    fill(frame, rect, " ", chrome());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(chrome_border())
        .style(chrome())
        .title(" TUI Settings ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.is_empty() {
        return;
    }
    let mut y = inner.y;
    let mut section = "";
    for (index, (row_section, label)) in crate::SETTINGS.iter().enumerate() {
        if *row_section != section {
            if y >= inner.bottom() {
                break;
            }
            section = row_section;
            draw_text(
                frame,
                Rect::new(inner.x, y, inner.width, 1),
                &format!(" {}", section.to_lowercase()),
                chrome_dim(),
            );
            y += 1;
        }
        if y >= inner.bottom() {
            break;
        }
        let (value, on) = crate::setting_value(settings, ui.sidebar_visible, index);
        let selected = index == ui.settings_index;
        let style = if selected {
            chrome_selected()
        } else {
            chrome()
        };
        let row = Rect::new(inner.x, y, inner.width, 1);
        fill(frame, row, " ", style);
        // A cursor on the left, the value on the right: the eye scans one
        // column for "where am I" and another for "what is it set to".
        draw_text(
            frame,
            row,
            &format!("  {} {}", if selected { "▸" } else { " " }, label),
            style,
        );
        let reading = if value.is_empty() {
            if on { "✓ On" } else { "  Off" }.to_string()
        } else {
            value
        };
        let width = unicode_column_width(&reading, None).min(u16::MAX as usize) as u16;
        if width + 2 < row.width {
            draw_text(
                frame,
                Rect::new(row.right() - width - 1, row.y, width, 1),
                &reading,
                style,
            );
        }
        y += 1;
    }
    if inner.height >= 2 {
        let footer = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
        fill(frame, footer, " ", chrome());
        draw_text(frame, footer, " ↑↓ move · ←→ change", chrome_dim());
        let done = " Done ";
        let width = unicode_column_width(done, None) as u16;
        if width + 2 < footer.width {
            draw_text(
                frame,
                Rect::new(footer.right() - width - 1, footer.y, width, 1),
                done,
                chrome_accent(),
            );
        }
    }
}

fn render_connections(frame: &mut Frame<'_>, ui: &UiState, view: &ViewLayout) {
    let Some(rect) = view.dialog else {
        return;
    };
    frame.render_widget(Clear, rect);
    fill(frame, rect, " ", chrome());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(chrome_border())
        .style(chrome())
        .title(" Connections ");
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.height == 0 {
        return;
    }
    draw_text(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        " Enter connect/retry · d disconnect · Esc close",
        chrome_dim(),
    );
    for (index, item) in ui.connections.iter().enumerate() {
        let y = inner.y.saturating_add(1 + index as u16);
        if y >= inner.bottom() {
            break;
        }
        let marker = match item.status {
            ConnectionStatus::Attached => "●",
            ConnectionStatus::Connecting | ConnectionStatus::Reconnecting => "…",
            ConnectionStatus::Failed => "!",
            ConnectionStatus::Unsupported => "-",
            ConnectionStatus::Disconnected => "○",
        };
        let detail = if item.detail.is_empty() {
            String::new()
        } else {
            format!(" · {}", item.detail)
        };
        let text = format!(" {marker} {}{detail}", item.label);
        let style = if index == ui.connection_index {
            chrome_selected()
        } else {
            match item.status {
                ConnectionStatus::Attached => chrome_success(),
                ConnectionStatus::Connecting | ConnectionStatus::Reconnecting => chrome_warning(),
                ConnectionStatus::Failed => chrome_danger(),
                ConnectionStatus::Unsupported => chrome_dim(),
                ConnectionStatus::Disconnected => chrome(),
            }
        };
        let row = Rect::new(inner.x, y, inner.width, 1);
        fill(frame, row, " ", style);
        draw_text(frame, row, &text, style);
    }
}

fn render_help(frame: &mut Frame<'_>, screen: Rect) {
    let width = screen.width.saturating_sub(4).clamp(1, 72);
    let height = screen.height.saturating_sub(2).clamp(1, 22);
    let rect = Rect::new(
        screen.x + screen.width.saturating_sub(width) / 2,
        screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    fill(frame, rect, " ", chrome());
    let help = [
        "ThinkTerm TUI",
        "",
        "Mouse: click thread/tab/pane · drag select · right-click actions",
        "Shift+mouse: local selection when an app captures the mouse",
        "",
        "Ctrl-b ?       help            Ctrl-b q/d     detach",
        "Ctrl-b b       sidebar         Ctrl-b g       navigator",
        "Ctrl-b C       connections     Ctrl-b Tab     next pane",
        "Ctrl-b c       new tab         Ctrl-b n/p     next/prev tab",
        "Ctrl-b 1..9    select tab      Ctrl-b hjkl    focus pane",
        "Ctrl-b v/-     split           Ctrl-b x       close pane",
        "Ctrl-b z       zoom            Ctrl-b r       resize mode",
        "Ctrl-b [       copy mode       Ctrl-b o       next attention",
        "",
        "Copy mode: hjkl/arrows move · Space select · / search · y copy",
        "Esc closes the current mode or dialog.",
    ]
    .join("\n");
    frame.render_widget(
        Paragraph::new(help)
            .style(chrome())
            .alignment(Alignment::Left)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(chrome_border())
                    .style(chrome())
                    .title(" Help "),
            ),
        rect,
    );
}

fn render_center(frame: &mut Frame<'_>, area: Rect, text: &str, style: Style) {
    if area.is_empty() {
        return;
    }
    let width = unicode_column_width(text, None).min(area.width as usize) as u16;
    let target = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height / 2,
        width,
        1,
    );
    draw_text(frame, target, text, style);
}

fn fill(frame: &mut Frame<'_>, area: Rect, symbol: &str, style: Style) {
    let buffer = frame.buffer_mut();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buffer[(x, y)].set_symbol(symbol).set_style(style);
        }
    }
}

fn draw_text(frame: &mut Frame<'_>, area: Rect, text: &str, style: Style) {
    if area.is_empty() {
        return;
    }
    frame.buffer_mut().set_stringn(
        area.x,
        area.y,
        fit_text(text, area.width as usize),
        area.width as usize,
        style,
    );
}

fn fit_text(text: &str, width: usize) -> String {
    let mut result = String::new();
    for ch in text.chars() {
        let mut candidate = result.clone();
        candidate.push(ch);
        if unicode_column_width(&candidate, None) > width {
            break;
        }
        result.push(ch);
    }
    result
}

fn chrome() -> Style {
    let palette = crate::settings::palette();
    if palette.reset_chrome {
        Style::reset()
    } else {
        Style::reset().fg(palette.foreground).bg(palette.background)
    }
}

fn chrome_dim() -> Style {
    let palette = crate::settings::palette();
    if palette.reset_chrome {
        Style::reset().add_modifier(Modifier::DIM)
    } else {
        Style::reset().fg(palette.dim).bg(palette.background)
    }
}

fn chrome_surface() -> Style {
    let palette = crate::settings::palette();
    Style::reset().fg(palette.foreground).bg(palette.surface)
}

fn chrome_border() -> Style {
    let palette = crate::settings::palette();
    Style::reset().fg(palette.border).bg(palette.background)
}

fn chrome_success() -> Style {
    let palette = crate::settings::palette();
    Style::reset().fg(palette.success).bg(palette.background)
}

fn chrome_warning() -> Style {
    let palette = crate::settings::palette();
    Style::reset().fg(palette.warning).bg(palette.background)
}

fn chrome_danger() -> Style {
    let palette = crate::settings::palette();
    Style::reset().fg(palette.danger).bg(palette.background)
}

fn chrome_bold() -> Style {
    chrome().add_modifier(Modifier::BOLD)
}

/// A filled button: the one command a sidebar leads with.
fn chrome_accent() -> Style {
    let palette = crate::settings::palette();
    if palette.reset_chrome {
        Style::reset().add_modifier(Modifier::BOLD)
    } else {
        Style::reset()
            .fg(palette.accent_foreground)
            .bg(palette.accent)
            .add_modifier(Modifier::BOLD)
    }
}

/// The accent colour as text on the ordinary background, for a status that has
/// to stand out from a row without taking the row over.
fn chrome_accent_text() -> Style {
    let palette = crate::settings::palette();
    Style::reset().fg(palette.accent).bg(palette.background)
}

fn chrome_selected() -> Style {
    let palette = crate::settings::palette();
    if palette.reset_chrome {
        Style::reset().add_modifier(Modifier::REVERSED)
    } else {
        Style::reset()
            .fg(palette.accent_foreground)
            .bg(palette.accent)
    }
}

fn chrome_bold_selected() -> Style {
    chrome_selected().add_modifier(Modifier::BOLD)
}

fn cell_style(attrs: &CellAttributes) -> Style {
    let mut modifiers = Modifier::empty();
    match attrs.intensity() {
        Intensity::Bold => modifiers.insert(Modifier::BOLD),
        Intensity::Half => modifiers.insert(Modifier::DIM),
        Intensity::Normal => {}
    }
    if attrs.underline() != Underline::None {
        modifiers.insert(Modifier::UNDERLINED);
    }
    match attrs.blink() {
        Blink::Slow => modifiers.insert(Modifier::SLOW_BLINK),
        Blink::Rapid => modifiers.insert(Modifier::RAPID_BLINK),
        Blink::None => {}
    }
    if attrs.italic() {
        modifiers.insert(Modifier::ITALIC);
    }
    if attrs.reverse() {
        modifiers.insert(Modifier::REVERSED);
    }
    if attrs.strikethrough() {
        modifiers.insert(Modifier::CROSSED_OUT);
    }
    if attrs.invisible() {
        modifiers.insert(Modifier::HIDDEN);
    }
    Style::new()
        .fg(to_ratatui_color(attrs.foreground()))
        .bg(to_ratatui_color(attrs.background()))
        .add_modifier(modifiers)
}

/// Carry a cell's color across as the same *kind* of color it arrived as.
///
/// `Default` becomes Ratatui's `Reset` so the host terminal supplies its own
/// default, an index stays an index, and only a true color — the one form that
/// already names an absolute value — is passed through as RGB.
fn to_ratatui_color(color: ColorAttribute) -> Color {
    match color {
        ColorAttribute::Default => Color::Reset,
        ColorAttribute::PaletteIndex(index) => Color::Indexed(index),
        // The palette fallback exists for terminals without true color; the
        // host decides which of the two it can honour, so hand it the exact
        // color and let it downgrade.
        ColorAttribute::TrueColorWithPaletteFallback(color, _)
        | ColorAttribute::TrueColorWithDefaultFallback(color) => to_ratatui_rgb(color),
    }
}

fn to_ratatui_rgb(color: SrgbaTuple) -> Color {
    let (red, green, blue, _) = color.as_rgba_u8();
    Color::Rgb(red, green, blue)
}

#[cfg(test)]
mod tests {
    use super::*;

    static THEME_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn divider_test_pane(pane_id: PaneId, border: Option<Rect>) -> PaneView {
        PaneView {
            pane_id,
            rect: border.unwrap_or_default(),
            scrollbar: None,
            nav: None,
            border,
            collapsed: None,
        }
    }

    #[test]
    fn mixed_pane_frames_keep_the_uncovered_divider() {
        let split = SplitView {
            index: 0,
            rect: Rect::new(5, 0, 1, 4),
            direction: SplitDirection::Horizontal,
        };
        let mut view = ViewLayout::default();
        view.panes = vec![
            divider_test_pane(1, Some(Rect::new(0, 0, 5, 4))),
            divider_test_pane(2, Some(Rect::new(6, 0, 5, 4))),
        ];
        assert!(!split_cell_needs_divider(&view, &split, 5, 2));

        view.panes[1].border = None;
        assert!(split_cell_needs_divider(&view, &split, 5, 2));
    }

    #[test]
    fn fit_text_respects_cell_width() {
        assert_eq!(fit_text("abc", 2), "ab");
        assert_eq!(fit_text("你好x", 4), "你好");
    }

    #[test]
    fn terminal_default_chrome_uses_host_colors() {
        let _guard = THEME_TEST_LOCK.lock().unwrap();
        crate::settings::set_active_theme(crate::settings::ThemeName::Terminal);
        for style in [
            chrome(),
            chrome_dim(),
            chrome_bold(),
            chrome_selected(),
            chrome_bold_selected(),
        ] {
            assert!(style.fg.is_none() || style.fg == Some(Color::Reset));
            assert!(style.bg.is_none() || style.bg == Some(Color::Reset));
        }
    }

    #[test]
    fn named_theme_chrome_uses_its_palette() {
        let _guard = THEME_TEST_LOCK.lock().unwrap();
        crate::settings::set_active_theme(crate::settings::ThemeName::Catppuccin);
        assert_eq!(chrome().fg, Some(Color::Rgb(205, 214, 244)));
        // The chrome shares the terminal's background rather than declaring one
        // of its own, so the sidebar cannot end up a different shade from the
        // panes beside it.
        assert_eq!(chrome().bg, Some(Color::Reset));
        assert_eq!(chrome_selected().bg, Some(Color::Rgb(137, 180, 250)));
    }

    #[test]
    fn handoff_overlay_clears_previously_rendered_terminal_cells() {
        let _guard = THEME_TEST_LOCK.lock().unwrap();
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(44, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                fill(frame, area, "S", chrome());
                render_handoff(
                    frame,
                    area,
                    "Terminal is being used on devbox",
                    "Click or scroll to continue",
                    HandoffAnimationFrame::default(),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains('S'));
        assert!(rendered.contains("Terminal is being used on devbox"));
        assert!(rendered.contains("Click or scroll to continue"));
    }

    #[test]
    fn roomy_handoff_overlay_draws_centered_eyes() {
        let _guard = THEME_TEST_LOCK.lock().unwrap();
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(50, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_handoff(
                    frame,
                    frame.area(),
                    "Terminal is being used on devbox",
                    "Click or scroll to continue",
                    HandoffAnimationFrame::default(),
                );
            })
            .unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("•  •"));
        assert!(!rendered.contains('╭'));
        assert!(!rendered.contains(">_"));
    }

    #[test]
    fn handoff_animation_blinks_the_eyes_briefly() {
        let ordinary = HandoffAnimationFrame::at(std::time::Duration::from_millis(750));
        assert!(!ordinary.eyes_closed);

        let blink = HandoffAnimationFrame::at(std::time::Duration::from_millis(4_800));
        assert!(blink.eyes_closed);
    }

    /// A program's `ESC[31m` has to reach the host terminal as "red", not as
    /// whatever RGB some palette happens to map index 1 to. Resolving it here
    /// is what let a red logo arrive in the default foreground.
    #[test]
    fn terminal_cells_forward_colors_without_resolving_them() {
        use wezterm_term::color::RgbColor;

        let defaults = cell_style(&CellAttributes::default());
        assert_eq!(defaults.fg, Some(Color::Reset));
        assert_eq!(defaults.bg, Some(Color::Reset));

        let ansi = CellAttributes::default()
            .set_foreground(ColorAttribute::PaletteIndex(1))
            .clone();
        assert_eq!(cell_style(&ansi).fg, Some(Color::Indexed(1)));

        // True color is the one form that already names an absolute color, so
        // it is the one form that survives as RGB.
        let exact = CellAttributes::default()
            .set_foreground(ColorAttribute::TrueColorWithDefaultFallback(
                RgbColor::new_8bpc(77, 88, 99).into(),
            ))
            .clone();
        assert_eq!(cell_style(&exact).fg, Some(Color::Rgb(77, 88, 99)));
    }
}
