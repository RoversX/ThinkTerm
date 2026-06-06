use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::ui::tokens::{
    CAPSULE_BORDER_WIDTH, ICON_BUTTON_BORDER_WIDTH, SIDEBAR_ICON_GAP, SIDEBAR_INSET,
    SIDEBAR_RESIZE_HANDLE_WIDTH, SIDEBAR_ROW_RADIUS, WINDOW_TAB_ADD_BUTTON_RADIUS,
};
use crate::termwindow::{
    RightSidebarMode, RightSidebarSnippetField, RightSidebarSnippetView, TermWindowNotif, UIItem,
    UIItemType,
};
use crate::ui::TextInputState;
use crate::ui::UiPalette;
use crate::utilsprites::RenderMetrics;
use anyhow::Context;
use config::keyassignment::ClipboardCopyDestination;
use std::rc::Rc;
use std::time::{Duration, Instant};
use termwiz::input::{KeyCode as TermKeyCode, Modifiers as TermModifiers};
use wezterm_font::LoadedFont;
use window::color::LinearRgba;
use window::{Clipboard, WindowOps};

const RIGHT_SIDEBAR_SECTION_GAP: usize = 12;
const RIGHT_SIDEBAR_WIDTH_CELLS: usize = 34;
const RIGHT_SIDEBAR_MIN_WIDTH: usize = 340;
const RIGHT_SIDEBAR_MAX_WIDTH: usize = 600;
const RIGHT_SIDEBAR_TOP_BAR_HEIGHT: usize = 82;
const RIGHT_SIDEBAR_CLOSE_BUTTON_SIZE: usize = 58;
const RIGHT_SIDEBAR_CLOSE_ICON_SIZE: usize = 27;
const RIGHT_SIDEBAR_CLOSE_BUTTON_X_ADJUST: usize = 8;
const RIGHT_SIDEBAR_CLOSE_BUTTON_Y_ADJUST: usize = 16;
const RIGHT_SIDEBAR_MODE_HEIGHT: usize = 72;
const RIGHT_SIDEBAR_EMPTY_HEIGHT: usize = 88;
const SNIPPET_TOOLBAR_HEIGHT: usize = 58;
const SNIPPET_SEARCH_HEIGHT: usize = 58;
const SNIPPET_CARD_HEIGHT: usize = 116;
const SNIPPET_EDITOR_HEADER_HEIGHT: usize = 108;
const SNIPPET_FIELD_HEIGHT: usize = 56;
const SNIPPET_BODY_FIELD_HEIGHT: usize = 190;
const SNIPPET_SAVE_BUTTON_HEIGHT: usize = 54;
const SNIPPET_ACTION_BUTTON_MIN_WIDTH: usize = 92;
const SNIPPET_ACTION_BUTTON_HEIGHT: usize = 46;
const SNIPPET_ROW_GAP: usize = 16;
const SNIPPET_LIST_TOP_GAP: usize = 18;
const SNIPPET_LIST_BOTTOM_PADDING: usize = 40;
const RIGHT_SIDEBAR_SCROLLBAR_VISIBLE_MS: u64 = 900;

#[derive(Debug, Clone, Copy)]
pub struct RightSidebarRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RightSidebarSnippetScrollGeometry {
    pub track_x: usize,
    pub track_y: usize,
    pub track_width: usize,
    pub track_height: usize,
    pub thumb_y: f32,
    pub thumb_height: f32,
    pub max_scroll: f32,
}

impl RightSidebarMode {
    fn icon(self) -> SvgIcon {
        match self {
            Self::Chat => SvgIcon::FolderTree,
            Self::Tasks => SvgIcon::NotebookTabs,
            Self::Snippets => SvgIcon::CodeXml,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Chat => "File",
            Self::Tasks => "Note",
            Self::Snippets => "Snippets",
        }
    }

    fn empty_label(self) -> &'static str {
        match self {
            Self::Chat => "Coming soon",
            Self::Tasks => "Coming soon",
            Self::Snippets => "No snippets",
        }
    }
}

pub fn right_sidebar_width_for_metrics(render_metrics: &RenderMetrics) -> usize {
    let default_width = (render_metrics.cell_size.width as usize * RIGHT_SIDEBAR_WIDTH_CELLS)
        .max(RIGHT_SIDEBAR_MIN_WIDTH);
    crate::native_settings::right_sidebar_width()
        .unwrap_or(default_width)
        .clamp(RIGHT_SIDEBAR_MIN_WIDTH, RIGHT_SIDEBAR_MAX_WIDTH)
}

impl crate::TermWindow {
    pub fn right_sidebar_width(&self) -> usize {
        if self.right_sidebar_collapsed {
            0
        } else {
            self.right_sidebar_width
                .clamp(RIGHT_SIDEBAR_MIN_WIDTH, self.right_sidebar_max_width())
        }
    }

    pub fn right_sidebar_max_width(&self) -> usize {
        RIGHT_SIDEBAR_MAX_WIDTH.min((self.dimensions.pixel_width / 2).max(RIGHT_SIDEBAR_MIN_WIDTH))
    }

    pub fn set_right_sidebar_width(&mut self, width: usize) {
        self.right_sidebar_width =
            width.clamp(RIGHT_SIDEBAR_MIN_WIDTH, self.right_sidebar_max_width());
    }

    pub fn persist_right_sidebar_width(&self) {
        let width = self
            .right_sidebar_width
            .clamp(RIGHT_SIDEBAR_MIN_WIDTH, RIGHT_SIDEBAR_MAX_WIDTH);
        if let Err(err) = crate::native_settings::save_right_sidebar_width(width) {
            log::warn!("failed to save right sidebar width: {err:#}");
        }
    }

    pub fn toggle_right_sidebar(&mut self) {
        self.right_sidebar_collapsed = !self.right_sidebar_collapsed;
    }

    pub fn expand_right_sidebar(&mut self) {
        self.right_sidebar_collapsed = false;
    }

    pub(crate) fn right_sidebar_toggle_icon(&self) -> SvgIcon {
        if self.right_sidebar_collapsed {
            SvgIcon::PanelRightOpen
        } else {
            SvgIcon::PanelRightClose
        }
    }

    pub(crate) fn right_sidebar_has_text_focus(&self) -> bool {
        self.right_sidebar_mode == RightSidebarMode::Snippets
            && self.right_sidebar_snippet_focus.is_some()
    }

    pub(crate) fn open_new_snippet_editor(&mut self) {
        self.right_sidebar_mode = RightSidebarMode::Snippets;
        self.right_sidebar_snippet_view = RightSidebarSnippetView::EditNew;
        self.right_sidebar_snippet_scroll_offset = 0.0;
        self.right_sidebar_snippet_title.clear();
        self.right_sidebar_snippet_body.clear();
        self.right_sidebar_snippet_focus = Some(RightSidebarSnippetField::Title);
    }

    pub(crate) fn open_existing_snippet_editor(&mut self, id: &str) {
        let Some(snippet) = crate::snippets::get_snippet(id) else {
            return;
        };
        self.right_sidebar_mode = RightSidebarMode::Snippets;
        self.right_sidebar_snippet_view = RightSidebarSnippetView::EditExisting(snippet.id);
        self.right_sidebar_snippet_scroll_offset = 0.0;
        self.right_sidebar_snippet_title.text = snippet.title;
        self.right_sidebar_snippet_title.selected_all = false;
        self.right_sidebar_snippet_body.text = snippet.body;
        self.right_sidebar_snippet_body.selected_all = false;
        self.right_sidebar_snippet_focus = Some(RightSidebarSnippetField::Title);
    }

    pub(crate) fn close_snippet_editor(&mut self) {
        self.right_sidebar_snippet_view = RightSidebarSnippetView::List;
        self.right_sidebar_snippet_focus = None;
        self.right_sidebar_snippet_scroll_offset = 0.0;
        self.right_sidebar_snippet_title.clear();
        self.right_sidebar_snippet_body.clear();
    }

    pub(crate) fn save_snippet_editor(&mut self) {
        let body = self.right_sidebar_snippet_body.text.trim().to_string();
        if body.is_empty() {
            self.right_sidebar_snippet_focus = Some(RightSidebarSnippetField::Body);
            return;
        }
        let title = self.right_sidebar_snippet_title.text.trim().to_string();
        let result = match self.right_sidebar_snippet_view.clone() {
            RightSidebarSnippetView::List => return,
            RightSidebarSnippetView::EditNew => {
                crate::snippets::create_snippet(title, body).map(Some)
            }
            RightSidebarSnippetView::EditExisting(id) => {
                crate::snippets::update_snippet(&id, title, body)
            }
        };
        match result {
            Ok(Some(_)) => self.close_snippet_editor(),
            Ok(None) => {
                self.right_sidebar_snippet_view = RightSidebarSnippetView::EditNew;
            }
            Err(err) => log::error!("failed to save snippet: {err:#}"),
        }
    }

    pub(crate) fn delete_snippet(&mut self, id: &str) {
        if let Err(err) = crate::snippets::delete_snippet(id) {
            log::error!("failed to delete snippet {id}: {err:#}");
        }
        if matches!(
            self.right_sidebar_snippet_view,
            RightSidebarSnippetView::EditExisting(ref editing_id) if editing_id == id
        ) {
            self.close_snippet_editor();
        }
        self.right_sidebar_snippet_scroll_offset = self
            .right_sidebar_snippet_scroll_offset
            .min(self.right_sidebar_snippet_scroll_max());
    }

    pub(crate) fn scroll_right_sidebar_snippets(&mut self, amount: i16) -> bool {
        if self.right_sidebar_collapsed
            || self.right_sidebar_mode != RightSidebarMode::Snippets
            || self.right_sidebar_snippet_view != RightSidebarSnippetView::List
            || amount == 0
        {
            return false;
        }

        let old = self.right_sidebar_snippet_scroll_offset;
        let max = self.right_sidebar_snippet_scroll_max();
        let steps = amount.unsigned_abs().max(1) as f32;
        let delta = (steps * 6.0).min(42.0);
        if amount < 0 {
            self.right_sidebar_snippet_scroll_offset =
                (self.right_sidebar_snippet_scroll_offset + delta).clamp(0.0, max);
        } else {
            self.right_sidebar_snippet_scroll_offset =
                (self.right_sidebar_snippet_scroll_offset - delta).clamp(0.0, max);
        }
        self.show_right_sidebar_snippet_scrollbar();
        (old - self.right_sidebar_snippet_scroll_offset).abs() > f32::EPSILON
    }

    pub(crate) fn paste_snippet_to_active_pane(&mut self, id: &str, run: bool) {
        let Some(snippet) = crate::snippets::get_snippet(id) else {
            return;
        };
        let Some(pane) = self.get_active_pane_or_overlay() else {
            return;
        };
        if let Err(err) = pane.send_paste(&snippet.body) {
            log::error!("failed to paste snippet {id}: {err:#}");
            return;
        }
        if run {
            if let Err(err) = pane.writer().write_all(b"\r") {
                log::error!("failed to run snippet {id}: {err:#}");
            }
        }
    }

    pub(crate) fn copy_right_sidebar_focused_input(&self) {
        let Some(text) = self
            .right_sidebar_focused_input()
            .map(|input| input.text.clone())
        else {
            return;
        };
        if !text.is_empty() {
            self.copy_to_clipboard(ClipboardCopyDestination::Clipboard, text);
        }
    }

    pub(crate) fn cut_right_sidebar_focused_input(&mut self) {
        let Some(input) = self.right_sidebar_focused_input_mut() else {
            return;
        };
        let text = if input.selected_all {
            input.take_selected_text().unwrap_or_default()
        } else {
            let text = input.text.clone();
            input.clear();
            text
        };
        if !text.is_empty() {
            self.copy_to_clipboard(ClipboardCopyDestination::Clipboard, text);
        }
    }

    pub(crate) fn paste_into_right_sidebar_from_clipboard(&mut self) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        let future = window.get_clipboard(Clipboard::Clipboard);
        promise::spawn::spawn(async move {
            if let Ok(text) = future.await {
                window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    if term_window.push_right_sidebar_text(&text) {
                        term_window.invalidate_window();
                    }
                })));
            }
        })
        .detach();
    }

    pub(crate) fn handle_right_sidebar_key(
        &mut self,
        key: TermKeyCode,
        mods: TermModifiers,
    ) -> bool {
        if !self.right_sidebar_has_text_focus() {
            return false;
        }

        if mods.contains(TermModifiers::SUPER) && !mods.contains(TermModifiers::ALT) {
            return match key {
                TermKeyCode::Char('a') | TermKeyCode::Char('A') => {
                    if let Some(input) = self.right_sidebar_focused_input_mut() {
                        input.select_all();
                    }
                    true
                }
                TermKeyCode::Char('c') | TermKeyCode::Char('C') => {
                    self.copy_right_sidebar_focused_input();
                    true
                }
                TermKeyCode::Char('x') | TermKeyCode::Char('X') => {
                    self.cut_right_sidebar_focused_input();
                    true
                }
                TermKeyCode::Char('v') | TermKeyCode::Char('V') => {
                    self.paste_into_right_sidebar_from_clipboard();
                    true
                }
                _ => false,
            };
        }

        if mods.intersects(TermModifiers::SUPER | TermModifiers::CTRL | TermModifiers::ALT) {
            return false;
        }

        match key {
            TermKeyCode::Escape => {
                self.right_sidebar_snippet_focus = None;
                true
            }
            TermKeyCode::Tab => {
                self.step_right_sidebar_snippet_field(if mods.contains(TermModifiers::SHIFT) {
                    -1
                } else {
                    1
                });
                true
            }
            TermKeyCode::Enter => {
                match self.right_sidebar_snippet_focus {
                    Some(RightSidebarSnippetField::Title) => {
                        self.right_sidebar_snippet_focus = Some(RightSidebarSnippetField::Body);
                    }
                    Some(RightSidebarSnippetField::Body) => {
                        self.push_right_sidebar_text("\n");
                    }
                    Some(RightSidebarSnippetField::Search) | None => {}
                }
                true
            }
            TermKeyCode::Backspace => {
                let reset_scroll =
                    self.right_sidebar_snippet_focus == Some(RightSidebarSnippetField::Search);
                if let Some(input) = self.right_sidebar_focused_input_mut() {
                    input.backspace();
                }
                if reset_scroll {
                    self.right_sidebar_snippet_scroll_offset = 0.0;
                }
                true
            }
            TermKeyCode::Char(ch) => {
                if !ch.is_control() {
                    self.push_right_sidebar_text(&ch.to_string());
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    pub(crate) fn push_right_sidebar_text(&mut self, text: &str) -> bool {
        match self.right_sidebar_snippet_focus {
            Some(RightSidebarSnippetField::Body) => {
                let input = &mut self.right_sidebar_snippet_body;
                if input.selected_all {
                    input.clear();
                }
                input.text.extend(
                    text.chars()
                        .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t'),
                );
                true
            }
            Some(_) => {
                let reset_scroll =
                    self.right_sidebar_snippet_focus == Some(RightSidebarSnippetField::Search);
                let Some(input) = self.right_sidebar_focused_input_mut() else {
                    return false;
                };
                input.push_text(text);
                if reset_scroll {
                    self.right_sidebar_snippet_scroll_offset = 0.0;
                }
                true
            }
            None => false,
        }
    }

    fn step_right_sidebar_snippet_field(&mut self, delta: isize) {
        let list_fields = [RightSidebarSnippetField::Search];
        let edit_fields = [
            RightSidebarSnippetField::Title,
            RightSidebarSnippetField::Body,
        ];
        let fields: &[RightSidebarSnippetField] = match self.right_sidebar_snippet_view {
            RightSidebarSnippetView::List => &list_fields,
            RightSidebarSnippetView::EditNew | RightSidebarSnippetView::EditExisting(_) => {
                &edit_fields
            }
        };
        let current = self.right_sidebar_snippet_focus.unwrap_or(fields[0]);
        let mut index = fields
            .iter()
            .position(|field| *field == current)
            .unwrap_or(0) as isize;
        index = (index + delta).rem_euclid(fields.len() as isize);
        self.right_sidebar_snippet_focus = Some(fields[index as usize]);
    }

    fn right_sidebar_focused_input(&self) -> Option<&TextInputState> {
        match self.right_sidebar_snippet_focus {
            Some(RightSidebarSnippetField::Search) => Some(&self.right_sidebar_snippet_search),
            Some(RightSidebarSnippetField::Title) => Some(&self.right_sidebar_snippet_title),
            Some(RightSidebarSnippetField::Body) => Some(&self.right_sidebar_snippet_body),
            None => None,
        }
    }

    fn right_sidebar_focused_input_mut(&mut self) -> Option<&mut TextInputState> {
        match self.right_sidebar_snippet_focus {
            Some(RightSidebarSnippetField::Search) => Some(&mut self.right_sidebar_snippet_search),
            Some(RightSidebarSnippetField::Title) => Some(&mut self.right_sidebar_snippet_title),
            Some(RightSidebarSnippetField::Body) => Some(&mut self.right_sidebar_snippet_body),
            None => None,
        }
    }

    pub fn right_sidebar_rect(&self) -> Option<RightSidebarRect> {
        let border = self.get_os_border();
        let bottom_tab_bar_height = if self.config.tab_bar_at_bottom && self.show_tab_bar {
            self.tab_bar_pixel_height().unwrap_or(0.0).ceil() as usize
        } else {
            0
        };
        let width = self.right_sidebar_width().min(
            self.dimensions
                .pixel_width
                .saturating_sub((border.left + border.right).get() as usize),
        );
        let y = border.top.get() as usize;
        let height = self
            .dimensions
            .pixel_height
            .saturating_sub(y + border.bottom.get() as usize + bottom_tab_bar_height);
        if width == 0 || height == 0 {
            return None;
        }

        let right_edge = self
            .dimensions
            .pixel_width
            .saturating_sub(border.right.get() as usize);
        Some(RightSidebarRect {
            x: right_edge.saturating_sub(width),
            y,
            width,
            height,
        })
    }

    fn right_sidebar_snippet_scroll_height(snippet_count: usize, viewport_height: usize) -> usize {
        let row_height = SNIPPET_CARD_HEIGHT + SNIPPET_ROW_GAP;
        let height = snippet_count
            .saturating_mul(row_height)
            .saturating_sub(SNIPPET_ROW_GAP);
        if height > viewport_height {
            height.saturating_add(SNIPPET_LIST_BOTTOM_PADDING)
        } else {
            height
        }
    }

    fn right_sidebar_snippet_scroll_metrics(&self) -> Option<(usize, usize, usize)> {
        let Some(rect) = self.right_sidebar_rect() else {
            return None;
        };
        if self.right_sidebar_collapsed
            || self.right_sidebar_mode != RightSidebarMode::Snippets
            || self.right_sidebar_snippet_view != RightSidebarSnippetView::List
        {
            return None;
        }

        let top_bar_y = rect.y + SIDEBAR_INSET * 2;
        let top_bar_height = RIGHT_SIDEBAR_TOP_BAR_HEIGHT.min(
            rect.y
                .saturating_add(rect.height)
                .saturating_sub(top_bar_y + SIDEBAR_INSET),
        );
        let content_top =
            top_bar_y + top_bar_height + RIGHT_SIDEBAR_MODE_HEIGHT + RIGHT_SIDEBAR_SECTION_GAP;
        let list_top =
            content_top + SNIPPET_TOOLBAR_HEIGHT.max(SNIPPET_SEARCH_HEIGHT) + SNIPPET_LIST_TOP_GAP;
        let content_bottom = rect.y.saturating_add(rect.height);
        let visible_height = content_bottom.saturating_sub(list_top + SIDEBAR_INSET);
        if visible_height == 0 {
            return None;
        }
        let snippet_count = self.filtered_snippet_count();
        let total_height = Self::right_sidebar_snippet_scroll_height(snippet_count, visible_height);
        Some((list_top, visible_height, total_height))
    }

    pub(crate) fn right_sidebar_snippet_scroll_max(&self) -> f32 {
        let Some((_, visible_height, total_height)) = self.right_sidebar_snippet_scroll_metrics()
        else {
            return 0.0;
        };
        total_height.saturating_sub(visible_height) as f32
    }

    pub(crate) fn right_sidebar_snippet_scroll_geometry(
        &self,
    ) -> Option<RightSidebarSnippetScrollGeometry> {
        let rect = self.right_sidebar_rect()?;
        let (list_top, visible_height, total_height) =
            self.right_sidebar_snippet_scroll_metrics()?;
        let max_scroll = total_height.saturating_sub(visible_height) as f32;
        if max_scroll <= 0.0 || total_height == 0 {
            return None;
        }

        let track_width = 4usize;
        let track_height = visible_height.max(1);
        let thumb_height = ((visible_height as f32 / total_height as f32) * track_height as f32)
            .clamp(28.0, track_height as f32);
        let travel = (track_height as f32 - thumb_height).max(1.0);
        let scroll_offset = self
            .right_sidebar_snippet_scroll_offset
            .clamp(0.0, max_scroll);
        let thumb_y = list_top as f32 + (scroll_offset / max_scroll) * travel;
        let track_x = rect
            .x
            .saturating_add(rect.width)
            .saturating_sub(SIDEBAR_INSET / 2 + track_width);

        Some(RightSidebarSnippetScrollGeometry {
            track_x,
            track_y: list_top,
            track_width,
            track_height,
            thumb_y,
            thumb_height,
            max_scroll,
        })
    }

    pub(crate) fn show_right_sidebar_snippet_scrollbar(&mut self) {
        self.right_sidebar_snippet_scrollbar_visible_until =
            Some(Instant::now() + Duration::from_millis(RIGHT_SIDEBAR_SCROLLBAR_VISIBLE_MS));
    }

    fn right_sidebar_snippet_scrollbar_visible(&self) -> bool {
        let actively_dragging = self.dragging.as_ref().is_some_and(|(item, _)| {
            matches!(
                item.item_type,
                UIItemType::RightSidebarSnippetScrollTrack
                    | UIItemType::RightSidebarSnippetScrollThumb
            )
        });
        actively_dragging
            || self
                .right_sidebar_snippet_scrollbar_visible_until
                .is_some_and(|until| until > Instant::now())
    }

    fn filtered_snippet_count(&self) -> usize {
        let needle = self
            .right_sidebar_snippet_search
            .text
            .trim()
            .to_ascii_lowercase();
        crate::snippets::list_snippets()
            .into_iter()
            .filter(|snippet| {
                needle.is_empty()
                    || snippet.title.to_ascii_lowercase().contains(&needle)
                    || snippet.body.to_ascii_lowercase().contains(&needle)
            })
            .count()
    }

    pub fn paint_right_sidebar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let rect = match self.right_sidebar_rect() {
            Some(rect) => rect,
            None => return Ok(()),
        };
        let chrome = UiPalette::for_appearance(crate::native_settings::effective_appearance());
        let foreground = chrome.text;
        let muted_fg = chrome.secondary_text;
        let sidebar_bg = chrome.workspace_sidebar_bg;
        let base_font_size = crate::native_settings::sidebar_font_size().clamp(12.0, 15.0);
        let ui_font = self
            .fonts
            .title_font_with_size(base_font_size)
            .context("right sidebar ui font")?;
        let ui_metrics = RenderMetrics::with_font_metrics(&ui_font.metrics());
        let ui_cell_height = ui_metrics.cell_size.height as usize;
        let icon_size = (ui_cell_height + 6).clamp(20, 24);

        if rect.y > 0 {
            self.filled_rectangle(
                layers,
                0,
                euclid::rect(rect.x as f32, 0.0, rect.width as f32, rect.y as f32),
                sidebar_bg,
            )
            .context("right sidebar top background")?;
        }
        self.filled_rectangle(
            layers,
            0,
            euclid::rect(
                rect.x as f32,
                rect.y as f32,
                rect.width as f32,
                rect.height as f32,
            ),
            sidebar_bg,
        )
        .context("right sidebar background")?;
        self.ui_items.push(UIItem {
            x: rect.x,
            y: 0,
            width: rect.width,
            height: rect.y.saturating_add(rect.height),
            item_type: UIItemType::RightSidebarBackground,
        });
        self.filled_rectangle(
            layers,
            1,
            euclid::rect(rect.x as f32, rect.y as f32, 1.0, rect.height as f32),
            chrome.separator,
        )
        .context("right sidebar separator")?;
        self.ui_items.push(UIItem {
            x: rect.x.saturating_sub(SIDEBAR_RESIZE_HANDLE_WIDTH / 2),
            y: rect.y,
            width: SIDEBAR_RESIZE_HANDLE_WIDTH,
            height: rect.height,
            item_type: UIItemType::RightSidebarResize,
        });

        let content_x = rect.x + SIDEBAR_INSET * 2;
        let content_width = rect.width.saturating_sub(SIDEBAR_INSET * 4);
        let top_bar_y = rect.y + SIDEBAR_INSET * 2;
        let top_bar_height = RIGHT_SIDEBAR_TOP_BAR_HEIGHT.min(
            rect.y
                .saturating_add(rect.height)
                .saturating_sub(top_bar_y + SIDEBAR_INSET),
        );
        if top_bar_height == 0 {
            return Ok(());
        }

        let close_button_size = RIGHT_SIDEBAR_CLOSE_BUTTON_SIZE
            .min(top_bar_height)
            .min(content_width)
            .max(1);
        let close_button_right_limit = rect
            .x
            .saturating_add(rect.width)
            .saturating_sub(SIDEBAR_INSET)
            .saturating_sub(close_button_size);
        let close_button_x = content_x
            .saturating_add(content_width)
            .saturating_sub(close_button_size)
            .saturating_add(RIGHT_SIDEBAR_CLOSE_BUTTON_X_ADJUST)
            .min(close_button_right_limit);
        let close_button_y = (top_bar_y + (top_bar_height.saturating_sub(close_button_size)) / 2)
            .saturating_sub(RIGHT_SIDEBAR_CLOSE_BUTTON_Y_ADJUST);
        let close_hovered = self.is_pointer_over_ui_rect(
            close_button_x,
            close_button_y,
            close_button_size,
            close_button_size,
        );
        let close_pressed = close_hovered
            && self.is_pointer_pressing_ui_rect(
                close_button_x,
                close_button_y,
                close_button_size,
                close_button_size,
            );
        let close_press_inset = if close_pressed { 1 } else { 0 };
        let close_visual_size = close_button_size.saturating_sub(close_press_inset * 2);
        if close_hovered || close_pressed {
            let close_fill = if close_pressed {
                chrome.control_pressed_bg
            } else {
                chrome.control_hover_bg
            };
            self.fill_rounded_rectangle_with_border(
                layers,
                2,
                euclid::rect(
                    (close_button_x + close_press_inset) as f32,
                    (close_button_y + close_press_inset) as f32,
                    close_visual_size as f32,
                    close_visual_size as f32,
                ),
                close_fill,
                foreground.mul_alpha(0.58),
                WINDOW_TAB_ADD_BUTTON_RADIUS,
                ICON_BUTTON_BORDER_WIDTH,
            )
            .context("right sidebar close button")?;
        }
        self.ui_items.push(UIItem {
            x: close_button_x,
            y: close_button_y,
            width: close_button_size,
            height: close_button_size,
            item_type: UIItemType::RightSidebarToggle,
        });
        let close_icon_size = RIGHT_SIDEBAR_CLOSE_ICON_SIZE
            .min(close_visual_size.saturating_sub(4))
            .max(1);
        self.paint_sidebar_icon(
            layers,
            self.right_sidebar_toggle_icon(),
            close_button_x
                + close_press_inset
                + (close_visual_size.saturating_sub(close_icon_size)) / 2,
            close_button_y
                + close_press_inset
                + (close_visual_size.saturating_sub(close_icon_size)) / 2,
            close_icon_size,
            if close_hovered { foreground } else { muted_fg },
        )?;

        let mode_y = top_bar_y + top_bar_height + RIGHT_SIDEBAR_SECTION_GAP;
        let mode_height = RIGHT_SIDEBAR_MODE_HEIGHT.min(
            rect.y
                .saturating_add(rect.height)
                .saturating_sub(mode_y + SIDEBAR_INSET),
        );
        if mode_height == 0 {
            return Ok(());
        }
        self.fill_rounded_rectangle_with_border(
            layers,
            1,
            euclid::rect(
                content_x as f32,
                mode_y as f32,
                content_width as f32,
                mode_height as f32,
            ),
            chrome.sidebar_button_bg,
            chrome.control_border,
            WINDOW_TAB_ADD_BUTTON_RADIUS,
            CAPSULE_BORDER_WIDTH,
        )
        .context("right sidebar mode selector")?;

        let modes = [
            RightSidebarMode::Chat,
            RightSidebarMode::Tasks,
            RightSidebarMode::Snippets,
        ];
        let mode_icon_size = (ui_cell_height + 12)
            .clamp(24, 30)
            .min(mode_height.saturating_sub(22))
            .max(1);
        let active_label_target_width = self
            .sidebar_text_width(&ui_font, self.right_sidebar_mode.label())?
            .ceil() as usize;
        let inactive_segment_min_width = (mode_icon_size + SIDEBAR_INSET * 4)
            .max(70)
            .min((content_width / modes.len()).max(1));
        let inactive_segment_count = modes.len().saturating_sub(1);
        let inactive_segments_min_width =
            inactive_segment_min_width.saturating_mul(inactive_segment_count);
        let active_segment_width =
            (mode_icon_size + SIDEBAR_ICON_GAP + active_label_target_width + SIDEBAR_INSET * 6)
                .min(
                    content_width
                        .saturating_sub(inactive_segments_min_width)
                        .max(1),
                )
                .max(1);
        let inactive_segments_width = content_width.saturating_sub(active_segment_width);
        let inactive_segment_width = if inactive_segment_count > 0 {
            inactive_segments_width / inactive_segment_count
        } else {
            0
        };
        let mut inactive_segment_remainder = if inactive_segment_count > 0 {
            inactive_segments_width % inactive_segment_count
        } else {
            0
        };
        let mut segment_x = content_x;
        for mode in modes.iter() {
            let mode = *mode;
            let active = mode == self.right_sidebar_mode;
            let segment_width = if active {
                active_segment_width
            } else {
                let extra = usize::from(inactive_segment_remainder > 0);
                inactive_segment_remainder = inactive_segment_remainder.saturating_sub(extra);
                inactive_segment_width.saturating_add(extra)
            };
            let remaining_width = content_x
                .saturating_add(content_width)
                .saturating_sub(segment_x);
            let segment_width = segment_width.min(remaining_width);
            let segment_right = segment_x.saturating_add(segment_width);
            if segment_width == 0 {
                continue;
            }

            let hovered =
                self.is_pointer_over_ui_rect(segment_x, mode_y, segment_width, mode_height);
            self.ui_items.push(UIItem {
                x: segment_x,
                y: mode_y,
                width: segment_width,
                height: mode_height,
                item_type: UIItemType::RightSidebarMode(mode),
            });

            if active {
                let inner_inset = 5;
                self.fill_rounded_rectangle_with_border(
                    layers,
                    2,
                    euclid::rect(
                        (segment_x + inner_inset) as f32,
                        (mode_y + inner_inset) as f32,
                        segment_width.saturating_sub(inner_inset * 2) as f32,
                        mode_height.saturating_sub(inner_inset * 2) as f32,
                    ),
                    chrome.control_bg,
                    chrome.control_border,
                    WINDOW_TAB_ADD_BUTTON_RADIUS,
                    CAPSULE_BORDER_WIDTH,
                )
                .context("right sidebar active mode")?;
            } else if hovered {
                self.fill_rounded_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        (segment_x + 5) as f32,
                        (mode_y + 5) as f32,
                        segment_width.saturating_sub(10) as f32,
                        mode_height.saturating_sub(10) as f32,
                    ),
                    chrome.sidebar_button_hover_bg,
                    WINDOW_TAB_ADD_BUTTON_RADIUS,
                )
                .context("right sidebar hovered mode")?;
            }

            let label_width = if active {
                segment_width
                    .saturating_sub(mode_icon_size + SIDEBAR_ICON_GAP + SIDEBAR_INSET * 2)
                    .min(active_label_target_width)
            } else {
                0
            };
            let total_width = if active {
                mode_icon_size + SIDEBAR_ICON_GAP + label_width
            } else {
                mode_icon_size
            };
            let icon_x = segment_x + (segment_width.saturating_sub(total_width) / 2);
            let icon_y =
                (mode_y + (mode_height.saturating_sub(mode_icon_size)) / 2).saturating_sub(1);
            self.paint_sidebar_icon(
                layers,
                mode.icon(),
                icon_x,
                icon_y,
                mode_icon_size,
                if active || hovered {
                    foreground
                } else {
                    muted_fg
                },
            )?;
            if active && label_width > 0 {
                self.paint_sidebar_text(
                    layers,
                    &ui_font,
                    ui_metrics,
                    mode.label(),
                    icon_x + mode_icon_size + SIDEBAR_ICON_GAP,
                    mode_y + (mode_height.saturating_sub(ui_cell_height)) / 2,
                    label_width,
                    foreground,
                )?;
            }
            segment_x = segment_right;
        }

        let content_top = mode_y + mode_height + RIGHT_SIDEBAR_SECTION_GAP;
        if self.right_sidebar_mode == RightSidebarMode::Snippets {
            self.paint_snippets_sidebar(
                layers,
                &ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                content_width,
                rect.y.saturating_add(rect.height),
                icon_size,
            )?;
            return Ok(());
        }

        let empty_top = content_top;
        let empty_height = RIGHT_SIDEBAR_EMPTY_HEIGHT.min(
            rect.y
                .saturating_add(rect.height)
                .saturating_sub(empty_top + SIDEBAR_INSET),
        );
        if empty_height == 0 {
            return Ok(());
        }
        self.fill_rounded_rectangle_with_border(
            layers,
            1,
            euclid::rect(
                content_x as f32,
                empty_top as f32,
                content_width as f32,
                empty_height as f32,
            ),
            chrome.sidebar_button_bg,
            chrome.control_border,
            SIDEBAR_ROW_RADIUS + 6.0,
            CAPSULE_BORDER_WIDTH,
        )
        .context("right sidebar empty state")?;
        let empty_icon_size = icon_size
            .min(22)
            .min(empty_height.saturating_sub(20))
            .max(1);
        let empty_icon_x = content_x + SIDEBAR_INSET + 2;
        let empty_icon_y = empty_top + (empty_height.saturating_sub(empty_icon_size)) / 2;
        self.paint_sidebar_icon(
            layers,
            self.right_sidebar_mode.icon(),
            empty_icon_x,
            empty_icon_y,
            empty_icon_size,
            muted_fg,
        )?;
        self.paint_sidebar_text(
            layers,
            &ui_font,
            ui_metrics,
            self.right_sidebar_mode.empty_label(),
            empty_icon_x + empty_icon_size + SIDEBAR_ICON_GAP + 2,
            empty_top + (empty_height.saturating_sub(ui_cell_height)) / 2,
            content_width.saturating_sub(empty_icon_size + SIDEBAR_ICON_GAP + SIDEBAR_INSET * 3),
            muted_fg,
        )?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_snippets_sidebar(
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
        icon_size: usize,
    ) -> anyhow::Result<()> {
        match self.right_sidebar_snippet_view.clone() {
            RightSidebarSnippetView::List => self.paint_snippets_list(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                content_top,
                content_width,
                content_bottom,
                icon_size,
            ),
            RightSidebarSnippetView::EditNew | RightSidebarSnippetView::EditExisting(_) => self
                .paint_snippet_editor(
                    layers,
                    ui_font,
                    ui_metrics,
                    chrome,
                    foreground,
                    muted_fg,
                    content_x,
                    content_top,
                    content_width,
                    content_bottom,
                    icon_size,
                ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_snippets_list(
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
        icon_size: usize,
    ) -> anyhow::Result<()> {
        let toolbar_y = content_top;
        let new_button_icon_size = (ui_metrics.cell_size.height as usize + 2).clamp(18, 22);
        let new_button_label_width =
            self.sidebar_text_width(ui_font, "New Snippet")?.ceil() as usize;
        let toolbar_gap = SNIPPET_ROW_GAP;
        let search_is_active = self.right_sidebar_snippet_focus
            == Some(RightSidebarSnippetField::Search)
            || !self.right_sidebar_snippet_search.text.is_empty();
        let full_new_button_width =
            new_button_icon_size + SIDEBAR_ICON_GAP + new_button_label_width + SIDEBAR_INSET * 4;
        let min_search_width = 160.min(content_width);
        let collapse_new_button = search_is_active
            || content_width < full_new_button_width + toolbar_gap + min_search_width;
        let new_button_width = if collapse_new_button {
            SNIPPET_TOOLBAR_HEIGHT.min(content_width)
        } else {
            full_new_button_width.min(content_width)
        };
        let new_button_label = if collapse_new_button {
            ""
        } else {
            "New Snippet"
        };
        self.paint_snippet_button(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            content_x,
            toolbar_y,
            new_button_width,
            SNIPPET_TOOLBAR_HEIGHT,
            Some(SvgIcon::CodeXml),
            new_button_label,
            UIItemType::RightSidebarSnippetNew,
            true,
        )?;

        let search_x = content_x + new_button_width + toolbar_gap;
        let search_y = toolbar_y;
        let search_width = content_x
            .saturating_add(content_width)
            .saturating_sub(search_x);
        if search_width > 28 {
            let search_input = self.right_sidebar_snippet_search.clone();
            self.paint_snippet_text_box(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                muted_fg,
                search_x,
                search_y,
                search_width,
                SNIPPET_SEARCH_HEIGHT,
                Some(SvgIcon::Search),
                "Search",
                &search_input,
                self.right_sidebar_snippet_focus == Some(RightSidebarSnippetField::Search),
                UIItemType::RightSidebarSnippetSearch,
                false,
            )?;
        }

        let list_top =
            toolbar_y + SNIPPET_TOOLBAR_HEIGHT.max(SNIPPET_SEARCH_HEIGHT) + SNIPPET_LIST_TOP_GAP;
        let needle = self
            .right_sidebar_snippet_search
            .text
            .trim()
            .to_ascii_lowercase();
        let snippets: Vec<_> = crate::snippets::list_snippets()
            .into_iter()
            .filter(|snippet| {
                needle.is_empty()
                    || snippet.title.to_ascii_lowercase().contains(&needle)
                    || snippet.body.to_ascii_lowercase().contains(&needle)
            })
            .collect();
        let row_height = SNIPPET_CARD_HEIGHT + SNIPPET_ROW_GAP;
        let visible_height = content_bottom.saturating_sub(list_top + SIDEBAR_INSET);
        let total_height =
            Self::right_sidebar_snippet_scroll_height(snippets.len(), visible_height);
        let max_scroll = total_height.saturating_sub(visible_height) as f32;
        self.right_sidebar_snippet_scroll_offset = self
            .right_sidebar_snippet_scroll_offset
            .clamp(0.0, max_scroll);
        let scroll_offset = self.right_sidebar_snippet_scroll_offset;

        if snippets.is_empty() {
            let empty_height = RIGHT_SIDEBAR_EMPTY_HEIGHT
                .min(content_bottom.saturating_sub(list_top + SIDEBAR_INSET));
            if empty_height == 0 {
                return Ok(());
            }
            self.fill_rounded_rectangle_with_border(
                layers,
                1,
                euclid::rect(
                    content_x as f32,
                    list_top as f32,
                    content_width as f32,
                    empty_height as f32,
                ),
                chrome.sidebar_button_bg,
                chrome.control_border,
                SIDEBAR_ROW_RADIUS + 6.0,
                CAPSULE_BORDER_WIDTH,
            )
            .context("right sidebar snippets empty state")?;
            let empty_icon_size = icon_size
                .min(22)
                .min(empty_height.saturating_sub(20))
                .max(1);
            let empty_icon_x = content_x + SIDEBAR_INSET + 2;
            let empty_icon_y = list_top + (empty_height.saturating_sub(empty_icon_size)) / 2;
            self.paint_sidebar_icon(
                layers,
                SvgIcon::CodeXml,
                empty_icon_x,
                empty_icon_y,
                empty_icon_size,
                muted_fg,
            )?;
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                if needle.is_empty() {
                    "No snippets"
                } else {
                    "No matching snippets"
                },
                empty_icon_x + empty_icon_size + SIDEBAR_ICON_GAP + 2,
                list_top + (empty_height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2,
                content_width
                    .saturating_sub(empty_icon_size + SIDEBAR_ICON_GAP + SIDEBAR_INSET * 3),
                muted_fg,
            )?;
            return Ok(());
        }

        let list_top_f = list_top as f32;
        let content_bottom = content_bottom.saturating_sub(SIDEBAR_INSET);
        let content_bottom_f = content_bottom as f32;
        for (idx, snippet) in snippets.into_iter().enumerate() {
            let row_top = list_top_f + (idx * row_height) as f32 - scroll_offset;
            let row_bottom = row_top + SNIPPET_CARD_HEIGHT as f32;
            if row_bottom <= list_top_f {
                continue;
            }
            if row_top >= content_bottom_f {
                break;
            }
            let y = row_top.floor().max(0.0) as usize;
            self.paint_snippet_card(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                content_x,
                y,
                content_width,
                &snippet,
                list_top,
                content_bottom,
            )?;
        }
        self.paint_right_sidebar_snippet_scrollbar(layers, chrome)?;
        Ok(())
    }

    fn paint_right_sidebar_snippet_scrollbar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
    ) -> anyhow::Result<()> {
        if self.right_sidebar_snippet_scrollbar_visible() {
            self.update_next_frame_time(self.right_sidebar_snippet_scrollbar_visible_until);
        }

        if !self.right_sidebar_snippet_scrollbar_visible() {
            return Ok(());
        }

        let Some(scroll) = self.right_sidebar_snippet_scroll_geometry() else {
            return Ok(());
        };

        let track_radius = scroll.track_width as f32 / 2.0;
        self.fill_rounded_rectangle(
            layers,
            2,
            euclid::rect(
                scroll.track_x as f32,
                scroll.track_y as f32,
                scroll.track_width as f32,
                scroll.track_height as f32,
            ),
            chrome.separator,
            track_radius,
        )
        .context("right sidebar snippet scroll track")?;

        self.fill_rounded_rectangle(
            layers,
            2,
            euclid::rect(
                scroll.track_x as f32,
                scroll.thumb_y,
                scroll.track_width as f32,
                scroll.thumb_height,
            ),
            chrome.scrollbar_thumb,
            track_radius,
        )
        .context("right sidebar snippet scroll thumb")?;

        let hit_slop = 6usize;
        self.ui_items.push(UIItem {
            x: scroll.track_x.saturating_sub(hit_slop),
            y: scroll.track_y,
            width: scroll.track_width + hit_slop * 2,
            height: scroll.track_height,
            item_type: UIItemType::RightSidebarSnippetScrollTrack,
        });
        self.ui_items.push(UIItem {
            x: scroll.track_x.saturating_sub(hit_slop),
            y: scroll.thumb_y.round().max(0.0) as usize,
            width: scroll.track_width + hit_slop * 2,
            height: scroll.thumb_height.round().max(1.0) as usize,
            item_type: UIItemType::RightSidebarSnippetScrollThumb,
        });

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_snippet_editor(
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
        icon_size: usize,
    ) -> anyhow::Result<()> {
        let header_y = content_top;
        let back_size = 44;
        let header_top = header_y + 8;
        self.paint_snippet_icon_button(
            layers,
            chrome,
            foreground,
            muted_fg,
            content_x,
            header_top,
            back_size,
            SvgIcon::ArrowLeft,
            UIItemType::RightSidebarSnippetBack,
        )?;

        let title_x = content_x + back_size + SIDEBAR_INSET;
        let save_label_width = self.sidebar_text_width(ui_font, "Save")?.ceil() as usize;
        let save_width = (save_label_width + SIDEBAR_INSET * 6)
            .clamp(110, 136)
            .min(content_width.saturating_sub(back_size + SIDEBAR_INSET * 2));
        let save_x = content_x + content_width.saturating_sub(save_width);
        let title_width = save_x.saturating_sub(title_x + SIDEBAR_INSET * 2);
        let title_label = match self.right_sidebar_snippet_view {
            RightSidebarSnippetView::EditNew => "New Snippet",
            RightSidebarSnippetView::EditExisting(_) => "Edit Snippet",
            RightSidebarSnippetView::List => "Snippet",
        };
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            title_label,
            title_x,
            header_top,
            title_width,
            foreground,
        )?;
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            "Personal vault",
            title_x,
            header_top + ui_metrics.cell_size.height as usize + 8,
            title_width,
            muted_fg,
        )?;

        self.paint_snippet_button(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            foreground,
            muted_fg,
            save_x,
            header_top,
            save_width,
            SNIPPET_SAVE_BUTTON_HEIGHT.min(SNIPPET_EDITOR_HEADER_HEIGHT - 12),
            None,
            "Save",
            UIItemType::RightSidebarSnippetSave,
            true,
        )?;

        let field_label_height = ui_metrics.cell_size.height as usize;
        let title_label_y = header_y + SNIPPET_EDITOR_HEADER_HEIGHT;
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            "Action description",
            content_x,
            title_label_y,
            content_width,
            muted_fg,
        )?;
        let title_y = title_label_y + field_label_height + 8;
        let title_input = self.right_sidebar_snippet_title.clone();
        self.paint_snippet_text_box(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            muted_fg,
            content_x,
            title_y,
            content_width,
            SNIPPET_FIELD_HEIGHT,
            None,
            "Describe this action",
            &title_input,
            self.right_sidebar_snippet_focus == Some(RightSidebarSnippetField::Title),
            UIItemType::RightSidebarSnippetTitle,
            false,
        )?;

        let body_label_y = title_y + SNIPPET_FIELD_HEIGHT + RIGHT_SIDEBAR_SECTION_GAP + 4;
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            "Script *",
            content_x,
            body_label_y,
            content_width,
            muted_fg,
        )?;
        let body_y = body_label_y + field_label_height + 8;
        let body_height = SNIPPET_BODY_FIELD_HEIGHT.min(content_bottom.saturating_sub(body_y));
        let body_input = self.right_sidebar_snippet_body.clone();
        self.paint_snippet_text_box(
            layers,
            ui_font,
            ui_metrics,
            chrome,
            muted_fg,
            content_x,
            body_y,
            content_width,
            body_height,
            None,
            "Type command or script",
            &body_input,
            self.right_sidebar_snippet_focus == Some(RightSidebarSnippetField::Body),
            UIItemType::RightSidebarSnippetBody,
            true,
        )?;

        // Keep icon_size meaningful in this signature for future toolbar actions.
        let _ = icon_size;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_snippet_card(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        x: usize,
        y: usize,
        width: usize,
        snippet: &crate::snippets::SnippetRecord,
        clip_top: usize,
        clip_bottom: usize,
    ) -> anyhow::Result<()> {
        let card_bottom = y.saturating_add(SNIPPET_CARD_HEIGHT);
        let visible_y = y.max(clip_top);
        let visible_bottom = card_bottom.min(clip_bottom);
        let visible_height = visible_bottom.saturating_sub(visible_y);
        if visible_height == 0 {
            return Ok(());
        }
        let hovered = self.is_pointer_over_ui_rect(x, visible_y, width, visible_height);
        let fill = if hovered {
            chrome.sidebar_button_hover_bg
        } else {
            chrome.sidebar_button_bg
        };
        self.fill_rounded_rectangle_with_border(
            layers,
            1,
            euclid::rect(
                x as f32,
                visible_y as f32,
                width as f32,
                visible_height as f32,
            ),
            fill,
            if hovered {
                chrome.control_border
            } else {
                chrome.control_border.mul_alpha(0.72)
            },
            SIDEBAR_ROW_RADIUS + 10.0,
            CAPSULE_BORDER_WIDTH,
        )
        .context("right sidebar snippet card")?;
        self.ui_items.push(UIItem {
            x,
            y: visible_y,
            width,
            height: visible_height,
            item_type: UIItemType::RightSidebarSnippetEdit(snippet.id.clone()),
        });

        let card_pad = SIDEBAR_INSET * 2;
        let text_x = x + card_pad;
        let run_button_width = (self.sidebar_text_width(ui_font, "Run")?.ceil() as usize
            + SIDEBAR_INSET * 4)
            .max(SNIPPET_ACTION_BUTTON_MIN_WIDTH);
        let paste_button_width = (self.sidebar_text_width(ui_font, "Paste")?.ceil() as usize
            + SIDEBAR_INSET * 4)
            .max(SNIPPET_ACTION_BUTTON_MIN_WIDTH);
        let delete_button_size = SNIPPET_ACTION_BUTTON_HEIGHT;
        let action_area_width = if hovered {
            run_button_width + paste_button_width + delete_button_size + SIDEBAR_INSET * 2
        } else {
            0
        };
        let title_y = y + SIDEBAR_INSET * 2;
        if title_y >= clip_top && title_y < clip_bottom {
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                &snippet.title,
                text_x,
                title_y,
                width.saturating_sub(card_pad * 2 + action_area_width),
                foreground,
            )?;
        }
        let preview = snippet_preview(&snippet.body);
        let preview_y = y + SIDEBAR_INSET * 2 + ui_metrics.cell_size.height as usize + 8;
        if preview_y >= clip_top && preview_y < clip_bottom {
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                &preview,
                text_x,
                preview_y,
                width.saturating_sub(card_pad * 2),
                muted_fg,
            )?;
        }

        let fully_visible = y >= clip_top && card_bottom <= clip_bottom;
        if hovered && fully_visible {
            let delete_x = x + width.saturating_sub(card_pad + delete_button_size);
            let paste_x = delete_x.saturating_sub(SIDEBAR_INSET + paste_button_width);
            let run_x = paste_x.saturating_sub(SIDEBAR_INSET + run_button_width);
            let action_y = y + SIDEBAR_INSET * 2 - 4;
            self.paint_snippet_button(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                run_x,
                action_y,
                run_button_width,
                SNIPPET_ACTION_BUTTON_HEIGHT,
                None,
                "Run",
                UIItemType::RightSidebarSnippetRun(snippet.id.clone()),
                true,
            )?;
            self.paint_snippet_button(
                layers,
                ui_font,
                ui_metrics,
                chrome,
                foreground,
                muted_fg,
                paste_x,
                action_y,
                paste_button_width,
                SNIPPET_ACTION_BUTTON_HEIGHT,
                None,
                "Paste",
                UIItemType::RightSidebarSnippetPaste(snippet.id.clone()),
                true,
            )?;
            self.paint_snippet_icon_button(
                layers,
                chrome,
                foreground,
                muted_fg,
                delete_x,
                action_y,
                delete_button_size,
                SvgIcon::Trash2,
                UIItemType::RightSidebarSnippetDelete(snippet.id.clone()),
            )?;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_snippet_text_box(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        muted_fg: LinearRgba,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        icon: Option<SvgIcon>,
        placeholder: &str,
        input: &TextInputState,
        focused: bool,
        item_type: UIItemType,
        multiline: bool,
    ) -> anyhow::Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        let hovered = self.is_pointer_over_ui_rect(x, y, width, height);
        let is_search_field = matches!(&item_type, UIItemType::RightSidebarSnippetSearch);
        self.fill_rounded_rectangle_with_border(
            layers,
            1,
            euclid::rect(x as f32, y as f32, width as f32, height as f32),
            chrome.control_bg,
            if focused {
                chrome.selected_bg
            } else if hovered {
                chrome.separator
            } else {
                chrome.control_border
            },
            if is_search_field {
                WINDOW_TAB_ADD_BUTTON_RADIUS
            } else {
                SIDEBAR_ROW_RADIUS + 4.0
            },
            CAPSULE_BORDER_WIDTH,
        )
        .context("right sidebar snippet text box")?;
        self.ui_items.push(UIItem {
            x,
            y,
            width,
            height,
            item_type,
        });

        let text_pad = SIDEBAR_INSET + 2;
        let mut text_x = x + text_pad;
        if let Some(icon) = icon {
            let icon_size = (ui_metrics.cell_size.height as usize + 2).clamp(18, 22);
            self.paint_sidebar_icon(
                layers,
                icon,
                text_x,
                y + (height.saturating_sub(icon_size)) / 2,
                icon_size,
                muted_fg,
            )?;
            text_x += icon_size + SIDEBAR_ICON_GAP;
        }

        let text_color = if input.text.is_empty() && !focused {
            muted_fg.mul_alpha(0.72)
        } else {
            chrome.text
        };
        let text = if input.text.is_empty() && !focused {
            placeholder
        } else {
            input.text.as_str()
        };
        if multiline {
            let line_height = ui_metrics.cell_size.height as usize + 4;
            let max_lines = height.saturating_sub(SIDEBAR_INSET * 2).max(1) / line_height.max(1);
            let mut line_y = y + SIDEBAR_INSET + 2;
            let mut last_line = "";
            let mut last_line_y = line_y;
            for line in text.lines().take(max_lines.max(1)) {
                last_line = line;
                last_line_y = line_y;
                self.paint_sidebar_text(
                    layers,
                    ui_font,
                    ui_metrics,
                    line,
                    text_x,
                    line_y,
                    width.saturating_sub((text_x - x) + text_pad),
                    text_color,
                )?;
                line_y += line_height;
            }
            if focused {
                let caret_x = text_x
                    + (self.sidebar_text_width(ui_font, last_line)?.ceil() as usize)
                        .min(width.saturating_sub((text_x - x) + text_pad));
                self.filled_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        caret_x as f32,
                        last_line_y as f32,
                        2.0,
                        (ui_metrics.cell_size.height as f32).max(1.0),
                    ),
                    chrome.selected_bg,
                )
                .context("right sidebar snippet body caret")?;
            }
        } else {
            self.paint_sidebar_text(
                layers,
                ui_font,
                ui_metrics,
                text,
                text_x,
                y + (height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2,
                width.saturating_sub((text_x - x) + text_pad),
                text_color,
            )?;
            if focused {
                let caret_x = text_x
                    + (self.sidebar_text_width(ui_font, text)?.ceil() as usize)
                        .min(width.saturating_sub((text_x - x) + text_pad));
                self.filled_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        caret_x as f32,
                        (y + (height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2)
                            as f32,
                        2.0,
                        (ui_metrics.cell_size.height as f32).max(1.0),
                    ),
                    chrome.selected_bg,
                )
                .context("right sidebar snippet text caret")?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_snippet_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        ui_font: &Rc<LoadedFont>,
        ui_metrics: RenderMetrics,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        icon: Option<SvgIcon>,
        label: &str,
        item_type: UIItemType,
        enabled: bool,
    ) -> anyhow::Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        let hovered = enabled && self.is_pointer_over_ui_rect(x, y, width, height);
        let pressed = hovered && self.is_pointer_pressing_ui_rect(x, y, width, height);
        let fill = if pressed {
            chrome.control_pressed_bg
        } else if hovered {
            chrome.control_hover_bg
        } else {
            chrome.sidebar_button_bg
        };
        self.fill_rounded_rectangle_with_border(
            layers,
            1,
            euclid::rect(x as f32, y as f32, width as f32, height as f32),
            fill,
            if hovered {
                chrome.control_border
            } else {
                chrome.control_border.mul_alpha(0.74)
            },
            WINDOW_TAB_ADD_BUTTON_RADIUS,
            CAPSULE_BORDER_WIDTH,
        )
        .context("right sidebar snippet button")?;
        if enabled {
            self.ui_items.push(UIItem {
                x,
                y,
                width,
                height,
                item_type,
            });
        }

        let icon_size = icon
            .map(|_| (ui_metrics.cell_size.height as usize + 2).clamp(18, 22))
            .unwrap_or(0);
        let horizontal_pad = SIDEBAR_INSET * 2;
        let available_label_width = width.saturating_sub(horizontal_pad * 2 + icon_size);
        let text_width =
            (self.sidebar_text_width(ui_font, label)?.ceil() as usize).min(available_label_width);
        let icon_label_gap = if icon.is_some() && text_width > 0 {
            SIDEBAR_ICON_GAP
        } else {
            0
        };
        let total_width = icon_size + icon_label_gap + text_width;
        let start_x =
            x + horizontal_pad + width.saturating_sub(horizontal_pad * 2 + total_width) / 2;
        let color = if enabled { foreground } else { muted_fg };
        let mut text_x = start_x;
        if let Some(icon) = icon {
            self.paint_sidebar_icon(
                layers,
                icon,
                text_x,
                y + (height.saturating_sub(icon_size)) / 2,
                icon_size,
                color,
            )?;
            text_x += icon_size + icon_label_gap;
        }
        self.paint_sidebar_text(
            layers,
            ui_font,
            ui_metrics,
            label,
            text_x,
            y + (height.saturating_sub(ui_metrics.cell_size.height as usize)) / 2,
            width.saturating_sub(text_x.saturating_sub(x) + SIDEBAR_INSET),
            color,
        )
    }

    fn paint_snippet_icon_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        chrome: UiPalette,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        x: usize,
        y: usize,
        size: usize,
        icon: SvgIcon,
        item_type: UIItemType,
    ) -> anyhow::Result<()> {
        let hovered = self.is_pointer_over_ui_rect(x, y, size, size);
        if hovered {
            self.fill_rounded_rectangle(
                layers,
                1,
                euclid::rect(x as f32, y as f32, size as f32, size as f32),
                chrome.control_hover_bg,
                WINDOW_TAB_ADD_BUTTON_RADIUS,
            )
            .context("right sidebar snippet icon button hover")?;
        }
        self.ui_items.push(UIItem {
            x,
            y,
            width: size,
            height: size,
            item_type,
        });
        let icon_size = (size * 58 / 100).max(16);
        self.paint_sidebar_icon(
            layers,
            icon,
            x + (size.saturating_sub(icon_size)) / 2,
            y + (size.saturating_sub(icon_size)) / 2,
            icon_size,
            if hovered { foreground } else { muted_fg },
        )
    }
}

fn snippet_preview(body: &str) -> String {
    body.lines()
        .find_map(|line| {
            let trimmed = line.trim();
            (!trimmed.is_empty()).then_some(trimmed)
        })
        .unwrap_or("")
        .chars()
        .take(96)
        .collect()
}
