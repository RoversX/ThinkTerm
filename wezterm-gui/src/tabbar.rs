use crate::termwindow::content_view::ContentViewId;
use crate::termwindow::ui::status_icon::UiStatusKind;
use crate::termwindow::ui::terminal_title_for_display;
use crate::termwindow::{PaneInformation, TabInformation, UIItem, UIItemType};
use config::{Config, ConfigHandle, TabBarColors};
use finl_unicode::grapheme_clusters::Graphemes;
use mlua::FromLua;
use termwiz::cell::{unicode_column_width, Cell, CellAttributes};
use termwiz::color::{AnsiColor, ColorSpec};
use termwiz::escape::csi::Sgr;
use termwiz::escape::parser::Parser;
use termwiz::escape::{Action, ControlCode, CSI};
use termwiz::surface::SEQ_ZERO;
use termwiz_funcs::{format_as_escapes, FormatColor, FormatItem};
use wezterm_term::{Line, Progress};
use window::{IntegratedTitleButton, IntegratedTitleButtonAlignment, IntegratedTitleButtonStyle};

const TERMINAL_TAB_ICON: &str = "\u{f120} ";

#[derive(Clone, Debug, PartialEq)]
pub struct TabBarState {
    line: Line,
    items: Vec<TabEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabBarItem {
    None,
    LeftStatus,
    RightStatus,
    Tab {
        tab_idx: usize,
        active: bool,
    },
    NewTabButton,
    WindowButton(IntegratedTitleButton),
    /// Synthetic tab for a ThinkTerm content view (e.g. SSH hosts).
    ContentView {
        id: ContentViewId,
    },
}

/// Which bar a [`TabBarState`] describes. The wezterm tab bar options and
/// `format-tab-title` configure the terminal bar only: Lua sees the terminal
/// area, and the window tab row around it is ThinkTerm's chrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabBarKind {
    /// ThinkTerm's window tab row, with its own titles.
    WindowTabs,
    /// The wezterm tab bar, drawn inside the terminal area.
    TerminalBar,
    /// The same place when only status text asked for it: no tabs.
    TerminalStatus,
}

impl TabBarKind {
    /// Whether titles take the fancy row's form rather than the retro one.
    fn fancy(self) -> bool {
        self == Self::WindowTabs
    }
}

/// Whether the terminal bar shows, and as what. The wezterm tab bar options
/// decide, as they would for wezterm's own bar: a retro bar shows its tabs; a
/// fancy one -- which the window tab row already is -- earns a line only for
/// status text.
pub(crate) fn terminal_bar_kind(
    config: &Config,
    num_tabs: usize,
    has_status: bool,
) -> Option<TabBarKind> {
    if !config.enable_tab_bar || (config.hide_tab_bar_if_only_one_tab && num_tabs <= 1) {
        return None;
    }
    if !config.use_fancy_tab_bar {
        Some(TabBarKind::TerminalBar)
    } else if has_status {
        Some(TabBarKind::TerminalStatus)
    } else {
        None
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TabEntry {
    pub item: TabBarItem,
    pub title: Line,
    pub status: Option<UiStatusKind>,
    x: usize,
    width: usize,
}

#[derive(Clone, Debug)]
struct TitleText {
    items: Vec<FormatItem>,
    len: usize,
}

/// The configuration as handed to Lua. `resolved_palette.tab_bar` is empty
/// unless the file or its scheme names tab bar colours, yet the bar is always
/// painted with some; plugins read them, so give them the ones in use.
pub(crate) fn config_for_lua(config: &ConfigHandle, tab_bar: Option<&TabBarColors>) -> Config {
    let mut config = (**config).clone();
    fill_tab_bar_colors(&mut config, tab_bar);
    config
}

fn fill_tab_bar_colors(config: &mut Config, tab_bar: Option<&TabBarColors>) {
    if config.resolved_palette.tab_bar.is_none() {
        config.resolved_palette.tab_bar = tab_bar.cloned();
    }
}

fn call_format_tab_title(
    tab: &TabInformation,
    tab_info: &[TabInformation],
    pane_info: &[PaneInformation],
    config: &ConfigHandle,
    tab_bar: &TabBarColors,
    hover: bool,
    tab_max_width: usize,
) -> Option<TitleText> {
    match config::run_immediate_with_lua_config(|lua| {
        if let Some(lua) = lua {
            let tabs = lua.create_sequence_from(tab_info.iter().cloned())?;
            let panes = lua.create_sequence_from(pane_info.iter().cloned())?;

            let v = config::lua::emit_sync_callback(
                &*lua,
                (
                    "format-tab-title".to_string(),
                    (
                        tab.clone(),
                        tabs,
                        panes,
                        config_for_lua(config, Some(tab_bar)),
                        hover,
                        tab_max_width,
                    ),
                ),
            )?;
            match &v {
                mlua::Value::Nil => Ok(None),
                mlua::Value::Table(_) => {
                    let items = <Vec<FormatItem>>::from_lua(v, &*lua)?;

                    let esc = format_as_escapes(items.clone())?;
                    let line = parse_status_text(&esc, CellAttributes::default());

                    Ok(Some(TitleText {
                        items,
                        len: line.len(),
                    }))
                }
                _ => {
                    let s = String::from_lua(v, &*lua)?;
                    let line = parse_status_text(&s, CellAttributes::default());
                    Ok(Some(TitleText {
                        len: line.len(),
                        items: vec![FormatItem::Text(s)],
                    }))
                }
            }
        } else {
            Ok(None)
        }
    }) {
        Ok(s) => s,
        Err(err) => {
            log::warn!("format-tab-title: {}", err);
            None
        }
    }
}

/// pct is a percentage in the range 0-100.
/// We want to map it to one of the nerdfonts:
///
/// * `md-checkbox_blank_circle_outline` (0xf0130) for an empty circle
/// * `md_circle_slice_1..=7` (0xf0a9e ..= 0xf0aa4) for a partly filled
///   circle
/// * `md_circle_slice_8` (0xf0aa5) for a filled circle
///
/// We use an empty circle for values close to 0%, a filled circle for values
/// close to 100%, and a partly filled circle for the rest (roughly evenly
/// distributed).
fn pct_to_glyph(pct: u8) -> char {
    match pct {
        0..=5 => '\u{f0130}',    // empty circle
        6..=18 => '\u{f0a9e}',   // centered at 12 (slightly smaller than 12.5)
        19..=31 => '\u{f0a9f}',  // centered at 25
        32..=43 => '\u{f0aa0}',  // centered at 37.5
        44..=56 => '\u{f0aa1}',  // half-filled circle, centered at 50
        57..=68 => '\u{f0aa2}',  // centered at 62.5
        69..=81 => '\u{f0aa3}',  // centered at 75
        82..=94 => '\u{f0aa4}',  // centered at 88 (slightly larger than 87.5)
        95..=100 => '\u{f0aa5}', // filled circle
        // Any other value is mapped to a filled circle.
        _ => '\u{f0aa5}',
    }
}

fn leading_legacy_progress_marker(line: &Line) -> bool {
    for cell in line.visible_cells() {
        let value = cell.str();
        if value.trim().is_empty() {
            continue;
        }
        return is_legacy_progress_marker(value);
    }
    false
}

fn is_legacy_progress_marker(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(ch) = chars.next() else {
        return false;
    };
    if chars.next().is_some() {
        return false;
    }

    matches!(
        ch as u32,
        0x2800..=0x28ff | 0xf0130 | 0xf0a9e..=0xf0aa5 | 0xee00..=0xee0b
    )
}

fn compute_tab_title(
    tab: &TabInformation,
    tab_info: &[TabInformation],
    pane_info: &[PaneInformation],
    config: &ConfigHandle,
    tab_bar: &TabBarColors,
    kind: TabBarKind,
    hover: bool,
    tab_max_width: usize,
) -> TitleText {
    let fancy = kind.fancy();
    let title = if kind == TabBarKind::WindowTabs {
        None
    } else {
        call_format_tab_title(
            tab,
            tab_info,
            pane_info,
            config,
            tab_bar,
            hover,
            tab_max_width,
        )
    };

    match title {
        Some(title) => title,
        None => {
            let mut items = vec![];
            let mut len = 0;

            if let Some(pane) = &tab.active_pane {
                let mut title = if tab.tab_title.is_empty() {
                    if fancy {
                        "Terminal".to_string()
                    } else {
                        terminal_title_for_display(&pane.title).to_string()
                    }
                } else {
                    tab.tab_title.clone()
                };

                let classic_spacing = if fancy { "" } else { " " };
                if config.show_tab_index_in_tab_bar && !fancy {
                    let index = format!(
                        "{classic_spacing}{}: ",
                        tab.tab_index
                            + if config.tab_and_split_indices_are_zero_based {
                                0
                            } else {
                                1
                            }
                    );
                    len += unicode_column_width(&index, None);
                    items.push(FormatItem::Text(index));

                    title = format!("{}{classic_spacing}", title);
                }

                if !fancy {
                    match pane.progress {
                        Progress::None => {}
                        Progress::Percentage(pct) | Progress::Error(pct) => {
                            let graphic = format!("{} ", pct_to_glyph(pct));
                            len += unicode_column_width(&graphic, None);
                            let color = if matches!(pane.progress, Progress::Percentage(_)) {
                                FormatItem::Foreground(FormatColor::AnsiColor(AnsiColor::Green))
                            } else {
                                FormatItem::Foreground(FormatColor::AnsiColor(AnsiColor::Red))
                            };
                            items.push(color);
                            items.push(FormatItem::Text(graphic));
                            items.push(FormatItem::Foreground(FormatColor::Default));
                        }
                        Progress::Indeterminate => {
                            // Classic tab titles keep the historical textual behavior.
                        }
                    }
                }

                if !fancy {
                    len += unicode_column_width(TERMINAL_TAB_ICON, None);
                    items.push(FormatItem::Text(TERMINAL_TAB_ICON.to_string()));
                }

                // We have a preferred soft minimum on tab width to make it
                // easier to click on tab titles, but we'll still go below
                // this if there are too many tabs to fit the window at
                // this width.
                if !fancy {
                    while len + unicode_column_width(&title, None) < 5 {
                        title.push(' ');
                    }
                }

                len += unicode_column_width(&title, None);
                items.push(FormatItem::Text(title));
            } else {
                let title = " no pane ".to_string();
                len += unicode_column_width(&title, None);
                items.push(FormatItem::Text(title));
            };

            TitleText { len, items }
        }
    }
}

fn is_tab_hover(mouse_x: Option<usize>, x: usize, tab_title_len: usize) -> bool {
    return mouse_x
        .map(|mouse_x| mouse_x >= x && mouse_x < x + tab_title_len)
        .unwrap_or(false);
}

fn visible_tab_range_from_scroll(
    tab_titles: &[TitleText],
    scroll_offset: usize,
    tab_width_max: usize,
    available_cells: usize,
) -> std::ops::Range<usize> {
    if tab_titles.is_empty() {
        return 0..0;
    }

    let widths: Vec<usize> = tab_titles
        .iter()
        .map(|title| title.len.min(tab_width_max).max(1))
        .collect();
    let total_width: usize = widths.iter().sum::<usize>() + tab_titles.len().saturating_sub(1);
    if total_width <= available_cells {
        return 0..tab_titles.len();
    }

    let mut start = scroll_offset.min(tab_titles.len().saturating_sub(1));
    let mut end = start;
    let mut used = 0usize;

    while end < widths.len() {
        let width = widths[end] + usize::from(used > 0);
        if used > 0 && used.saturating_add(width) > available_cells {
            break;
        }
        used += width;
        end += 1;
    }

    while start > 0 && end == widths.len() {
        let width = widths[start - 1] + usize::from(used > 0);
        if used.saturating_add(width) > available_cells {
            break;
        }
        used += width;
        start -= 1;
    }

    start..end.max(start + 1).min(tab_titles.len())
}

impl TabBarState {
    pub fn default() -> Self {
        Self {
            line: Line::with_width(1, SEQ_ZERO),
            items: vec![TabEntry {
                item: TabBarItem::None,
                title: Line::from_text(" ", &CellAttributes::blank(), 1, None),
                status: None,
                x: 1,
                width: 1,
            }],
        }
    }

    pub fn line(&self) -> &Line {
        &self.line
    }

    pub fn items(&self) -> &[TabEntry] {
        &self.items
    }

    fn integrated_title_buttons(
        mouse_x: Option<usize>,
        x: &mut usize,
        config: &ConfigHandle,
        fancy: bool,
        items: &mut Vec<TabEntry>,
        line: &mut Line,
        colors: &TabBarColors,
    ) {
        let default_cell = if fancy {
            CellAttributes::default()
        } else {
            colors.new_tab().as_cell_attributes()
        };

        let default_cell_hover = if fancy {
            CellAttributes::default()
        } else {
            colors.new_tab_hover().as_cell_attributes()
        };

        let window_hide =
            parse_status_text(&config.tab_bar_style.window_hide, default_cell.clone());
        let window_hide_hover = parse_status_text(
            &config.tab_bar_style.window_hide_hover,
            default_cell_hover.clone(),
        );

        let window_maximize =
            parse_status_text(&config.tab_bar_style.window_maximize, default_cell.clone());
        let window_maximize_hover = parse_status_text(
            &config.tab_bar_style.window_maximize_hover,
            default_cell_hover.clone(),
        );

        let window_close =
            parse_status_text(&config.tab_bar_style.window_close, default_cell.clone());
        let window_close_hover = parse_status_text(
            &config.tab_bar_style.window_close_hover,
            default_cell_hover.clone(),
        );

        for button in &config.integrated_title_buttons {
            use IntegratedTitleButton as Button;
            let title = match button {
                Button::Hide => {
                    let hover = is_tab_hover(mouse_x, *x, window_hide_hover.len());

                    if hover {
                        &window_hide_hover
                    } else {
                        &window_hide
                    }
                }
                Button::Maximize => {
                    let hover = is_tab_hover(mouse_x, *x, window_maximize_hover.len());

                    if hover {
                        &window_maximize_hover
                    } else {
                        &window_maximize
                    }
                }
                Button::Close => {
                    let hover = is_tab_hover(mouse_x, *x, window_close_hover.len());

                    if hover {
                        &window_close_hover
                    } else {
                        &window_close
                    }
                }
            };

            line.append_line(title.to_owned(), SEQ_ZERO);

            let width = title.len();
            items.push(TabEntry {
                item: TabBarItem::WindowButton(*button),
                title: title.to_owned(),
                status: None,
                x: *x,
                width,
            });

            *x += width;
        }
    }

    /// Build a new tab bar from the current state
    /// mouse_x is some if the mouse is on the same row as the tab bar.
    /// title_width is the total number of cell columns in the window.
    /// window allows access to the tabs associated with the window.
    pub fn new(
        title_width: usize,
        mouse_x: Option<usize>,
        tab_info: &[TabInformation],
        pane_info: &[PaneInformation],
        colors: Option<&TabBarColors>,
        config: &ConfigHandle,
        use_integrated_title_buttons: bool,
        tab_scroll_offset: f32,
        left_status: &str,
        right_status: &str,
        kind: TabBarKind,
        themed: bool,
    ) -> Self {
        let colors = colors.cloned().unwrap_or_else(TabBarColors::default);
        let fancy = kind.fancy();
        // Colours derived from the terminal are its own ground: leave those
        // cells on the default background, painted exactly as the terminal's.
        let ground = |attrs: CellAttributes| {
            let mut attrs = attrs;
            if themed {
                attrs.set_background(ColorSpec::Default);
            }
            attrs
        };

        let active_cell_attrs = ground(colors.active_tab().as_cell_attributes());
        let inactive_hover_attrs = ground(colors.inactive_tab_hover().as_cell_attributes());
        let inactive_cell_attrs = ground(colors.inactive_tab().as_cell_attributes());
        let new_tab_hover_attrs = ground(colors.new_tab_hover().as_cell_attributes());
        let new_tab_attrs = ground(colors.new_tab().as_cell_attributes());

        let new_tab = parse_status_text(
            &config.tab_bar_style.new_tab,
            if fancy {
                CellAttributes::default()
            } else {
                new_tab_attrs.clone()
            },
        );
        let new_tab_hover = parse_status_text(
            &config.tab_bar_style.new_tab_hover,
            if fancy {
                CellAttributes::default()
            } else {
                new_tab_hover_attrs.clone()
            },
        );

        // We ultimately want to produce a line looking like this:
        // ` | tab1-title x | tab2-title x |  +      . - X `
        // Where the `+` sign will spawn a new tab (or show a context
        // menu with tab creation options) and the other three chars
        // are symbols representing minimize, maximize and close.

        let mut active_tab_no = 0;

        // `show_tabs_in_tab_bar` is a wezterm option: the window tab row keeps
        // its tabs whatever it says.
        let show_tabs = match kind {
            TabBarKind::WindowTabs => true,
            TabBarKind::TerminalBar => config.show_tabs_in_tab_bar,
            TabBarKind::TerminalStatus => false,
        };
        let tab_titles: Vec<TitleText> = if show_tabs {
            tab_info
                .iter()
                .map(|tab| {
                    if tab.is_active {
                        active_tab_no = tab.tab_index;
                    }
                    compute_tab_title(
                        tab,
                        tab_info,
                        pane_info,
                        config,
                        &colors,
                        kind,
                        false,
                        config.tab_max_width,
                    )
                })
                .collect()
        } else {
            vec![]
        };
        let number_of_tabs = tab_titles.len();
        let show_new_tab_button = false;

        let black_cell = Cell::blank_with_attrs(ground(
            CellAttributes::default()
                .set_background(ColorSpec::TrueColor(*colors.background()))
                .clone(),
        ));
        // The left status leads the line; the tabs get what it leaves.
        let left_status_line = parse_status_text(left_status, black_cell.attrs().clone());

        let available_cells = title_width.saturating_sub(
            left_status_line.len()
                + number_of_tabs.saturating_sub(1)
                + if show_new_tab_button {
                    new_tab.len()
                } else {
                    0
                },
        );
        let tab_width_max = config.tab_max_width;
        let visible_tab_range = if fancy {
            0..tab_titles.len()
        } else {
            // Nothing scrolls the terminal bar, so it follows the active tab:
            // the first offset that keeps it in view.
            let mut offset = tab_scroll_offset.max(0.0).floor() as usize;
            let mut range =
                visible_tab_range_from_scroll(&tab_titles, offset, tab_width_max, available_cells);
            while !range.contains(&active_tab_no) && offset < active_tab_no {
                offset += 1;
                range = visible_tab_range_from_scroll(
                    &tab_titles,
                    offset,
                    tab_width_max,
                    available_cells,
                );
            }
            range
        };

        let mut line = Line::with_width(0, SEQ_ZERO);

        let mut x = 0;
        let mut items = vec![];

        if use_integrated_title_buttons
            && config.integrated_title_button_style != IntegratedTitleButtonStyle::MacOsNative
            && config.integrated_title_button_alignment == IntegratedTitleButtonAlignment::Left
        {
            Self::integrated_title_buttons(
                mouse_x, &mut x, config, fancy, &mut items, &mut line, &colors,
            );
        }

        if left_status_line.len() > 0 {
            items.push(TabEntry {
                item: TabBarItem::LeftStatus,
                title: left_status_line.clone(),
                status: None,
                x,
                width: left_status_line.len(),
            });
            x += left_status_line.len();
            line.append_line(left_status_line, SEQ_ZERO);
        }

        for (tab_idx, tab_title) in tab_titles.iter().enumerate() {
            if !visible_tab_range.contains(&tab_idx) {
                continue;
            }

            let tab_title_len = tab_title.len.min(tab_width_max).min(available_cells.max(1));
            let active = tab_idx == active_tab_no;
            let hover = !active && is_tab_hover(mouse_x, x, tab_title_len);

            // Recompute the title so that it factors in both the hover state
            // and the adjusted maximum tab width based on available space.
            let tab_title = compute_tab_title(
                &tab_info[tab_idx],
                tab_info,
                pane_info,
                config,
                &colors,
                kind,
                hover,
                tab_title_len,
            );

            let cell_attrs = if active {
                &active_cell_attrs
            } else if hover {
                &inactive_hover_attrs
            } else {
                &inactive_cell_attrs
            };

            let tab_start_idx = x;

            let esc = format_as_escapes(tab_title.items.clone()).expect("already parsed ok above");
            let mut tab_line = parse_status_text(
                &esc,
                if fancy {
                    CellAttributes::default()
                } else {
                    cell_attrs.clone()
                },
            );

            let title = tab_line.clone();
            let status = if fancy {
                tab_info[tab_idx]
                    .active_pane
                    .as_ref()
                    .and_then(|pane| UiStatusKind::from_progress(&pane.progress))
                    .or_else(|| {
                        leading_legacy_progress_marker(&tab_line).then_some(UiStatusKind::Running)
                    })
            } else {
                None
            };
            if tab_line.len() > tab_width_max {
                tab_line.resize(tab_width_max, SEQ_ZERO);
            }

            let width = tab_line.len();

            items.push(TabEntry {
                item: TabBarItem::Tab { tab_idx, active },
                title,
                status,
                x: tab_start_idx,
                width,
            });

            line.append_line(tab_line, SEQ_ZERO);
            x += width;
        }

        // New tab button
        if config.show_new_tab_button_in_tab_bar && show_new_tab_button {
            let hover = is_tab_hover(mouse_x, x, new_tab_hover.len());

            let new_tab_button = if hover { &new_tab_hover } else { &new_tab };

            let button_start = x;
            let width = new_tab_button.len();

            line.append_line(new_tab_button.clone(), SEQ_ZERO);

            items.push(TabEntry {
                item: TabBarItem::NewTabButton,
                title: new_tab_button.clone(),
                status: None,
                x: button_start,
                width,
            });

            x += width;
        }

        // Reserve place for integrated title buttons
        let title_width = if use_integrated_title_buttons
            && config.integrated_title_button_style != IntegratedTitleButtonStyle::MacOsNative
            && config.integrated_title_button_alignment == IntegratedTitleButtonAlignment::Right
        {
            let window_hide =
                parse_status_text(&config.tab_bar_style.window_hide, CellAttributes::default());
            let window_hide_hover = parse_status_text(
                &config.tab_bar_style.window_hide_hover,
                CellAttributes::default(),
            );

            let window_maximize = parse_status_text(
                &config.tab_bar_style.window_maximize,
                CellAttributes::default(),
            );
            let window_maximize_hover = parse_status_text(
                &config.tab_bar_style.window_maximize_hover,
                CellAttributes::default(),
            );
            let window_close = parse_status_text(
                &config.tab_bar_style.window_close,
                CellAttributes::default(),
            );
            let window_close_hover = parse_status_text(
                &config.tab_bar_style.window_close_hover,
                CellAttributes::default(),
            );

            let hide_len = window_hide.len().max(window_hide_hover.len());
            let maximize_len = window_maximize.len().max(window_maximize_hover.len());
            let close_len = window_close.len().max(window_close_hover.len());

            let mut width_to_reserve = 0;
            for button in &config.integrated_title_buttons {
                use IntegratedTitleButton as Button;
                let button_len = match button {
                    Button::Hide => hide_len,
                    Button::Maximize => maximize_len,
                    Button::Close => close_len,
                };
                width_to_reserve += button_len;
            }

            title_width.saturating_sub(width_to_reserve)
        } else {
            title_width
        };

        let status_space_available = title_width.saturating_sub(x);

        let mut right_status_line = parse_status_text(right_status, black_cell.attrs().clone());
        items.push(TabEntry {
            item: TabBarItem::RightStatus,
            title: right_status_line.clone(),
            status: None,
            x,
            width: status_space_available,
        });

        while right_status_line.len() > status_space_available {
            right_status_line.remove_cell(0, SEQ_ZERO);
        }

        line.append_line(right_status_line, SEQ_ZERO);
        while line.len() < title_width {
            line.insert_cell(x, black_cell.clone(), title_width, SEQ_ZERO);
        }

        if use_integrated_title_buttons
            && config.integrated_title_button_style != IntegratedTitleButtonStyle::MacOsNative
            && config.integrated_title_button_alignment == IntegratedTitleButtonAlignment::Right
        {
            x = title_width;
            Self::integrated_title_buttons(
                mouse_x, &mut x, config, fancy, &mut items, &mut line, &colors,
            );
        }

        Self { line, items }
    }

    pub fn compute_ui_items(
        &self,
        y: usize,
        cell_height: usize,
        cell_width: usize,
        x_offset: usize,
    ) -> Vec<UIItem> {
        let mut items = vec![];

        for entry in self.items.iter() {
            items.push(UIItem {
                x: x_offset + entry.x * cell_width,
                width: entry.width * cell_width,
                y,
                height: cell_height,
                item_type: UIItemType::TerminalBar(entry.item),
            });
        }

        items
    }
}

pub fn parse_status_text(text: &str, default_cell: CellAttributes) -> Line {
    let mut pen = default_cell.clone();
    let mut cells = vec![];
    let mut ignoring = false;
    let mut print_buffer = String::new();

    fn flush_print(buf: &mut String, cells: &mut Vec<Cell>, pen: &CellAttributes) {
        for g in Graphemes::new(buf.as_str()) {
            let cell = Cell::new_grapheme(g, pen.clone(), None);
            let width = cell.width();
            cells.push(cell);
            for _ in 1..width {
                // Line/Screen expect double wide graphemes to be followed by a blank in
                // the next column position, otherwise we'll render incorrectly
                cells.push(Cell::blank_with_attrs(pen.clone()));
            }
        }
        buf.clear();
    }

    let mut parser = Parser::new();
    parser.parse(text.as_bytes(), |action| {
        if ignoring {
            return;
        }
        match action {
            Action::Print(c) => print_buffer.push(c),
            Action::PrintString(s) => print_buffer.push_str(&s),
            Action::Control(c) => {
                flush_print(&mut print_buffer, &mut cells, &pen);
                match c {
                    ControlCode::CarriageReturn | ControlCode::LineFeed => {
                        ignoring = true;
                    }
                    _ => {}
                }
            }
            Action::CSI(csi) => {
                flush_print(&mut print_buffer, &mut cells, &pen);
                match csi {
                    CSI::Sgr(sgr) => match sgr {
                        Sgr::Reset => pen = default_cell.clone(),
                        Sgr::Intensity(i) => {
                            pen.set_intensity(i);
                        }
                        Sgr::Underline(u) => {
                            pen.set_underline(u);
                        }
                        Sgr::Overline(o) => {
                            pen.set_overline(o);
                        }
                        Sgr::VerticalAlign(o) => {
                            pen.set_vertical_align(o);
                        }
                        Sgr::Blink(b) => {
                            pen.set_blink(b);
                        }
                        Sgr::Italic(i) => {
                            pen.set_italic(i);
                        }
                        Sgr::Inverse(inverse) => {
                            pen.set_reverse(inverse);
                        }
                        Sgr::Invisible(invis) => {
                            pen.set_invisible(invis);
                        }
                        Sgr::StrikeThrough(strike) => {
                            pen.set_strikethrough(strike);
                        }
                        Sgr::Foreground(col) => {
                            if let ColorSpec::Default = col {
                                pen.set_foreground(default_cell.foreground());
                            } else {
                                pen.set_foreground(col);
                            }
                        }
                        Sgr::Background(col) => {
                            if let ColorSpec::Default = col {
                                pen.set_background(default_cell.background());
                            } else {
                                pen.set_background(col);
                            }
                        }
                        Sgr::UnderlineColor(col) => {
                            pen.set_underline_color(col);
                        }
                        Sgr::Font(_) => {}
                    },
                    _ => {}
                }
            }
            Action::OperatingSystemCommand(_)
            | Action::DeviceControl(_)
            | Action::Esc(_)
            | Action::KittyImage(_)
            | Action::XtGetTcap(_)
            | Action::Sixel(_) => {
                flush_print(&mut print_buffer, &mut cells, &pen);
            }
        }
    });
    flush_print(&mut print_buffer, &mut cells, &pen);
    Line::from_cells(cells, SEQ_ZERO)
}

#[cfg(test)]
mod test {
    use super::*;
    use config::RgbaColor;

    fn colors(rgb: (u8, u8, u8)) -> TabBarColors {
        TabBarColors {
            background: Some(RgbaColor::from(rgb)),
            ..Default::default()
        }
    }

    #[test]
    fn lua_sees_the_tab_bar_colours_in_use_when_none_are_configured() {
        let mut config = Config::default_config();
        config.resolved_palette.tab_bar = None;
        fill_tab_bar_colors(&mut config, Some(&colors((1, 2, 3))));
        assert_eq!(config.resolved_palette.tab_bar, Some(colors((1, 2, 3))));
    }

    #[test]
    fn terminal_bar_shows_what_the_wezterm_options_ask_for() {
        let mut config = Config::default_config();
        config.enable_tab_bar = true;
        config.hide_tab_bar_if_only_one_tab = false;
        config.use_fancy_tab_bar = true;
        // A fancy bar is the window tab row already; only status text adds a line.
        assert_eq!(terminal_bar_kind(&config, 2, false), None);
        assert_eq!(
            terminal_bar_kind(&config, 2, true),
            Some(TabBarKind::TerminalStatus)
        );
        config.use_fancy_tab_bar = false;
        assert_eq!(
            terminal_bar_kind(&config, 2, false),
            Some(TabBarKind::TerminalBar)
        );
        config.hide_tab_bar_if_only_one_tab = true;
        assert_eq!(terminal_bar_kind(&config, 1, true), None);
        assert_eq!(
            terminal_bar_kind(&config, 2, true),
            Some(TabBarKind::TerminalBar)
        );
        config.enable_tab_bar = false;
        assert_eq!(terminal_bar_kind(&config, 2, true), None);
    }

    #[test]
    fn configured_tab_bar_colours_reach_lua_unchanged() {
        let mut config = Config::default_config();
        config.resolved_palette.tab_bar = Some(colors((9, 9, 9)));
        fill_tab_bar_colors(&mut config, Some(&colors((1, 2, 3))));
        assert_eq!(config.resolved_palette.tab_bar, Some(colors((9, 9, 9))));
    }
}
