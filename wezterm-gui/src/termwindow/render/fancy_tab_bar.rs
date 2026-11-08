use crate::quad::{
    HeapQuadAllocator, QuadClipRect, QuadEdgeFade, QuadTrait, TripleLayerQuadAllocator,
    TripleLayerQuadAllocatorTrait,
};
use crate::shapecache::{BorrowedShapeCacheKey, ShapedInfo};
use crate::tabbar::{TabBarItem, TabEntry};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::ui::platform_chrome::uses_integrated_window_buttons;
use crate::termwindow::ui::tokens::{
    ICON_BUTTON_BORDER_WIDTH, SIDEBAR_INSET, TAB_CLOSE_HOVER_INSET, TAB_CLOSE_HOVER_RADIUS,
    TAB_CONTENT_INSET, TAB_ICON_SIZE, TAB_ROW_FADE_WIDTH, TAB_ROW_START_PADDING, TAB_TITLE_FADE_GAP,
    TAB_VERTICAL_PADDING, TITLE_FADE, WINDOW_TAB_ADD_BUTTON_RADIUS,
    WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_Y_OFFSET, WINDOW_TAB_GAP,
    WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE, WINDOW_TAB_LEADING_ACTION_GAP,
    WINDOW_TAB_LEADING_ACTION_ICON_SIZE, WINDOW_TAB_RADIUS, WINDOW_TAB_TOP_SPACER,
};
use crate::termwindow::window_tab_reveal::PaneBarWindowChrome;
use crate::termwindow::{TermWindowNotif, UIItem, UIItemType, UiShapeCacheLookup};
use crate::ui::draw::TextFade;
use crate::ui::UiPalette;
use crate::utilsprites::RenderMetrics;
use anyhow::{anyhow, Context};
use std::rc::Rc;
use termwiz::cell::CellAttributes;
use wezterm_bidi::Direction;
use wezterm_font::{ClearShapeCache, LoadedFont};
use wezterm_term::Line;
use window::color::LinearRgba;
use window::WindowOps;
use window::{IntegratedTitleButton, IntegratedTitleButtonStyle, WindowState};

const WINDOW_TAB_INSET: usize = 8;
const WINDOW_TAB_ICON_GAP: usize = 8;
const WINDOW_TAB_MIN_TEXT_COLS: usize = 3;
const UI_SHAPE_CACHE_FONT_IDENTITY_BIT: u64 = 1u64 << 63;
/// The largest recording of a tab row kept for the next frame.
pub(crate) const TAB_ROW_SCRATCH_KEEP_BYTES: usize = 1 << 20;


impl crate::TermWindow {
    pub fn invalidate_fancy_tab_bar(&mut self) {
        self.fancy_tab_bar.take();
    }

    pub fn paint_fancy_tab_bar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
        defer_trailing_actions: bool,
    ) -> anyhow::Result<Vec<UIItem>> {
        let chrome = self.chrome();
        let row_height = self.tab_bar_pixel_height()?.ceil() as usize;
        if row_height == 0 || !self.window_tab_row_drawn() {
            return Ok(vec![]);
        }

        let font = self
            .fonts
            .title_font_with_size(crate::native_settings::tab_font_size())?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let tab_width = self.window_tab_width_pixels().ceil() as usize;

        let background = self.tab_bar_surface(chrome.header_bg);
        let foreground = chrome.text;
        let muted_fg = chrome.secondary_text;

        let border = self.get_os_border();
        let row_x = self.tab_bar_left_edge();
        let row_y = border.top.get() as usize;
        let content_top_spacer = self.ui_px(WINDOW_TAB_TOP_SPACER).min(row_height);
        let content_row_y = row_y + content_top_spacer;
        let content_row_height = row_height.saturating_sub(content_top_spacer);
        let icon_size = self.ui_px(TAB_ICON_SIZE).min(content_row_height);
        let button_size = content_row_height
            .saturating_sub(self.ui_px(TAB_VERTICAL_PADDING) * 2)
            .max(icon_size);
        // Past the window's edge, by as much as the Library pushes it.
        let row_width = (self
            .dimensions
            .pixel_width
            .saturating_sub(row_x + border.right.get() as usize)
            .saturating_sub(self.right_sidebar_width())
            + self.library_push())
        .max(1);
        let row_right = row_x + row_width;
        let viewport_left = (row_x as f32 + self.window_tab_left_padding_pixels()).ceil() as usize
            + self.ui_px(TAB_ROW_START_PADDING);
        let viewport_right =
            row_right.saturating_sub(self.window_tab_trailing_action_reserved_width());
        let viewport_width = viewport_right.saturating_sub(viewport_left);

        self.filled_rectangle(
            layers,
            0,
            euclid::rect(
                row_x as f32,
                row_y as f32,
                row_width as f32,
                row_height as f32,
            ),
            background,
        )
        .context("fancy tab bar background")?;

        let mut ui_items = vec![UIItem {
            x: row_x,
            y: row_y,
            width: row_width,
            height: row_height,
            item_type: UIItemType::TabBar(TabBarItem::None),
        }];

        self.paint_window_tab_leading_actions(
            layers,
            &mut ui_items,
            row_x,
            content_row_y,
            content_row_height,
            foreground,
            muted_fg,
        )?;

        let show_pane_layer_divider = mux::Mux::get()
            .get_active_tab_for_window(self.mux_window_id)
            .is_some_and(|tab| tab.iter_panes_ignoring_zoom().len() > 1);
        if show_pane_layer_divider {
            let divider_y = row_y + row_height;
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(row_x as f32, divider_y as f32, row_width as f32, 1.0),
                // The shared separator token, same as the sidebar edges draw:
                // this used to be a one-off `text * 0.14`, which read markedly
                // brighter than every other rule in the window.
                chrome.separator,
            )
            .context("fancy tab bar pane layer divider")?;
        }

        if viewport_width == 0 || tab_width == 0 {
            return Ok(ui_items);
        }

        let max_scroll = self.max_window_tab_scroll_offset();
        let scroll_offset = self.tab_bar_scroll_offset.clamp(0.0, max_scroll.max(0.0));
        let inline_rename_tab_idx = self.inline_window_tab_rename_tab_id().and_then(|tab_id| {
            mux::Mux::get()
                .get_window(self.mux_window_id)
                .and_then(|window| window.idx_by_id(tab_id))
        });

        // Remote thread connection views are selected from the sidebar and must
        // not expose the underlying mux tabs here; clicking those stale tabs
        // would leave the remote thread UI and jump to the prior terminal.
        let active_content_view_id = self.active_content_view_id;
        let cv_active = self.active_content_view_shown_in_tab_bar();
        let show_mux_tabs = !self.active_content_view_is_remote_thread();

        // The tabs are recorded, so that where the row runs on past its
        // viewport it can fade out in their own alpha: over a see-through
        // bar a fade of its ground would show as a band.
        let into = &mut *layers;
        let mut row = std::mem::take(&mut self.tab_row_scratch);
        row.recycle();
        let row_start = row.mark();
        let layers = &mut TripleLayerQuadAllocator::Heap(&mut row);

        let tab_step = tab_width + self.ui_px(WINDOW_TAB_GAP);
        let mut tab_sequence_idx = 0usize;
        if show_mux_tabs {
            for item in self.tab_bar.items() {
                let TabBarItem::Tab { tab_idx, active } = item.item else {
                    continue;
                };
                let active = active && !cv_active;

                let virtual_left = tab_sequence_idx as f32 * tab_step as f32 - scroll_offset;
                tab_sequence_idx += 1;

                let tab_left = viewport_left as f32 + virtual_left;
                let tab_right = tab_left + tab_width as f32;
                if tab_right <= viewport_left as f32 || tab_left >= viewport_right as f32 {
                    continue;
                }

                let visible_left = tab_left.max(viewport_left as f32);
                let visible_right = tab_right.min(viewport_right as f32);
                let visible_width = (visible_right - visible_left).max(0.0);
                if visible_width <= 1.0 {
                    continue;
                }

                self.paint_window_tab(
                    layers,
                    &mut ui_items,
                    item,
                    tab_idx,
                    active,
                    inline_rename_tab_idx == Some(tab_idx),
                    tab_left,
                    visible_left,
                    visible_width,
                    viewport_left,
                    viewport_right,
                    content_row_y,
                    content_row_height,
                    tab_width,
                    button_size,
                    icon_size,
                    &font,
                    metrics,
                    chrome,
                    background,
                    if active { foreground } else { muted_fg },
                )?;
            }
        }

        // Synthetic content-view tabs (e.g. Remote Hosts), placed after the mux tabs.
        for content_tab in &self.content_views {
            if !self.content_view_shown_in_tab_bar(content_tab) {
                continue;
            }
            let virtual_left = tab_sequence_idx as f32 * tab_step as f32 - scroll_offset;
            tab_sequence_idx += 1;
            let tab_left = viewport_left as f32 + virtual_left;
            let tab_right = tab_left + tab_width as f32;
            if tab_right > viewport_left as f32 && tab_left < viewport_right as f32 {
                let visible_left = tab_left.max(viewport_left as f32);
                let visible_right = tab_right.min(viewport_right as f32);
                let visible_width = (visible_right - visible_left).max(0.0);
                if visible_width > 1.0 {
                    let active = active_content_view_id == Some(content_tab.id);
                    self.paint_content_view_tab(
                        layers,
                        &mut ui_items,
                        content_tab.id,
                        &content_tab.view.title(),
                        active,
                        tab_left,
                        visible_left,
                        visible_width,
                        viewport_left,
                        viewport_right,
                        content_row_y,
                        content_row_height,
                        tab_width,
                        button_size,
                        icon_size,
                        &font,
                        metrics,
                        chrome,
                        background,
                        if active { foreground } else { muted_fg },
                    )?;
                }
            }
        }

        // Dissolve the row into the bar wherever it runs on past the viewport,
        // so a clipped tab reads as continuing rather than as one sliced in
        // half. What follows paints straight into the bar again.
        let layers = into;
        self.apply_tab_row(
            row,
            &row_start,
            &mut *layers,
            viewport_left,
            viewport_right,
            scroll_offset > 0.5,
            scroll_offset < max_scroll - 0.5,
        )?;

        // Establish a hard paint boundary before the trailing actions. Tab
        // surfaces and glyphs are already geometrically clipped, but this mask
        // also protects the action area from shader and texture overhang.
        if viewport_right < row_right && !self.chrome_see_through() {
            self.filled_rectangle(
                layers,
                2,
                euclid::rect(
                    viewport_right as f32,
                    row_y as f32,
                    row_right.saturating_sub(viewport_right) as f32,
                    row_height as f32,
                ),
                background,
            )
            .context("fancy tab bar trailing action mask")?;
        }

        if !defer_trailing_actions {
            ui_items.extend(self.paint_window_tab_trailing_actions(layers)?);
        }

        Ok(ui_items)
    }

    /// Replay a tab `row` recorded from `start` into `layers`, cut at its
    /// viewport and fading out at the ends that run on past it; then keep
    /// its buffer for the next row, unless it grew large.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_tab_row(
        &mut self,
        mut row: HeapQuadAllocator,
        start: &crate::quad::HeapQuadMark,
        layers: &mut TripleLayerQuadAllocator,
        viewport_left: usize,
        viewport_right: usize,
        fade_at_left: bool,
        fade_at_right: bool,
    ) -> anyhow::Result<()> {
        let width = self
            .ui_px(TAB_ROW_FADE_WIDTH)
            .min(viewport_right.saturating_sub(viewport_left) / 2) as f32;
        row.cut_after(
            start,
            QuadClipRect::from_top_left_pixels(
                viewport_left as f32,
                0.0,
                viewport_right as f32,
                self.dimensions.pixel_height as f32,
                &self.dimensions,
            ),
            QuadEdgeFade {
                left: if fade_at_left { width } else { 0.0 },
                right: if fade_at_right { width } else { 0.0 },
                ..Default::default()
            },
        );
        let applied = row.apply_to(layers);
        if row.resident_bytes() <= TAB_ROW_SCRATCH_KEEP_BYTES {
            self.tab_row_scratch = row;
        }
        applied
    }

    pub(crate) fn paint_window_tab_trailing_actions(
        &self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<Vec<UIItem>> {
        let row_height = self.tab_bar_pixel_height()?.ceil() as usize;
        if row_height == 0 || !self.window_tab_row_drawn() {
            return Ok(Vec::new());
        }
        let border = self.get_os_border();
        let content_top_spacer = self.ui_px(WINDOW_TAB_TOP_SPACER).min(row_height);
        let content_row_y = border.top.get() as usize + content_top_spacer;
        let content_row_height = row_height.saturating_sub(content_top_spacer);
        let icon_size = self.ui_px(TAB_ICON_SIZE).min(content_row_height);
        let button_size = content_row_height
            .saturating_sub(self.ui_px(TAB_VERTICAL_PADDING) * 2)
            .max(icon_size);
        let row_x = self.tab_bar_left_edge();
        // Past the window's edge, by as much as the Library pushes it.
        let row_width = (self
            .dimensions
            .pixel_width
            .saturating_sub(row_x + border.right.get() as usize)
            .saturating_sub(self.right_sidebar_width())
            + self.library_push())
        .max(1);
        let row_right = row_x + row_width;
        let chrome = self.chrome();
        let foreground = chrome.text;
        let muted_fg = chrome.secondary_text;
        let mut ui_items = Vec::new();
        let mut action_right = row_right.saturating_sub(self.ui_px(WINDOW_TAB_INSET) + 2);
        // Over an open right sidebar the window's buttons are the sidebar's:
        // see `paint_right_sidebar_window_buttons`. One sliding away holds
        // them where they are, which is where the row's go once it is gone.
        if self.fancy_tab_bar_shows_window_buttons() && self.right_sidebar_width() == 0 {
            action_right = if self.right_sidebar_holds_window_buttons() {
                action_right.saturating_sub(self.window_tab_window_buttons_width(button_size))
            } else {
                self.paint_window_tab_window_buttons(
                    layers,
                    &mut ui_items,
                    action_right,
                    content_row_y,
                    content_row_height,
                    button_size,
                    icon_size,
                    foreground,
                    muted_fg,
                )?
            };
        }

        // An auto-hiding row's pin takes the new-tab button's slot, just
        // right of it, and moves the button one slot left.
        let pin_then_new_tab = |slot: usize| {
            if self.window_tab_reveal.shows_pin() {
                (
                    Some(slot),
                    slot.saturating_sub(self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP) + button_size),
                )
            } else {
                (None, slot)
            }
        };
        let pin_x;
        if self.right_sidebar_width() > 0 {
            let action_button_x = action_right.saturating_sub(button_size);
            if cfg!(target_os = "macos") {
                let (pin, new_button_x) = pin_then_new_tab(action_button_x);
                pin_x = pin;
                self.paint_window_tab_icon_button(
                    layers,
                    &mut ui_items,
                    new_button_x,
                    content_row_y,
                    content_row_height,
                    button_size,
                    icon_size,
                    foreground,
                    muted_fg,
                    SvgIcon::Plus,
                    UIItemType::TabBar(TabBarItem::NewTabButton),
                    false,
                )?;
            } else {
                let right_sidebar_toggle_x = action_button_x;
                let (pin, new_button_x) = pin_then_new_tab(
                    right_sidebar_toggle_x
                        .saturating_sub(self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP) + button_size),
                );
                pin_x = pin;
                self.paint_window_tab_icon_button(
                    layers,
                    &mut ui_items,
                    new_button_x,
                    content_row_y,
                    content_row_height,
                    button_size,
                    icon_size,
                    foreground,
                    muted_fg,
                    SvgIcon::Plus,
                    UIItemType::TabBar(TabBarItem::NewTabButton),
                    false,
                )?;
                self.paint_window_tab_right_sidebar_toggle_button(
                    layers,
                    &mut ui_items,
                    right_sidebar_toggle_x,
                    content_row_y,
                    content_row_height,
                    button_size,
                    icon_size,
                    foreground,
                    muted_fg,
                )?;
            }
        } else {
            let right_sidebar_toggle_x = action_right.saturating_sub(button_size);
            // With every sidebar panel off the toggle paints nothing, and
            // window_tab_trailing_action_reserved_width reserves a single
            // slot to match. The new button has to take that slot, or it
            // lands one button to the left -- inside the width the tab strip
            // was just given -- and overlaps the last tab.
            let (pin, new_button_x) = pin_then_new_tab(if self.right_sidebar_has_panels() {
                right_sidebar_toggle_x
                    .saturating_sub(self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP) + button_size)
            } else {
                right_sidebar_toggle_x
            });
            pin_x = pin;
            self.paint_window_tab_icon_button(
                layers,
                &mut ui_items,
                new_button_x,
                content_row_y,
                content_row_height,
                button_size,
                icon_size,
                foreground,
                muted_fg,
                SvgIcon::Plus,
                UIItemType::TabBar(TabBarItem::NewTabButton),
                false,
            )?;
            self.paint_window_tab_right_sidebar_toggle_button(
                layers,
                &mut ui_items,
                right_sidebar_toggle_x,
                content_row_y,
                content_row_height,
                button_size,
                icon_size,
                foreground,
                muted_fg,
            )?;
        }

        if let Some(pin_x) = pin_x {
            // In, the row stays out and keeps its place, and the pin is drawn
            // in the text's colour; out, the row comes and goes with the
            // pointer.
            self.paint_window_tab_icon_button(
                layers,
                &mut ui_items,
                pin_x,
                content_row_y,
                content_row_height,
                button_size,
                icon_size,
                foreground,
                muted_fg,
                SvgIcon::Pin,
                UIItemType::WindowTabsPin,
                self.window_tab_reveal.pinned(),
            )?;
        }

        Ok(ui_items)
    }

    /// The window's own buttons while an open right sidebar holds them at its
    /// top rather than the row at its end. They stay there whether the row
    /// is shown, hidden or on its way: hidden with the row, a window that
    /// draws its own frame could not be closed.
    pub(crate) fn paint_right_sidebar_window_buttons(
        &self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<Vec<UIItem>> {
        let row_height = self.tab_bar_pixel_height()?.ceil() as usize;
        if row_height == 0 || !self.right_sidebar_holds_window_buttons() {
            return Ok(Vec::new());
        }
        let border = self.get_os_border();
        let content_top_spacer = self.ui_px(WINDOW_TAB_TOP_SPACER).min(row_height);
        let content_row_y = border.top.get() as usize + content_top_spacer;
        let content_row_height = row_height.saturating_sub(content_top_spacer);
        let icon_size = self.ui_px(TAB_ICON_SIZE).min(content_row_height);
        let button_size = content_row_height
            .saturating_sub(self.ui_px(TAB_VERTICAL_PADDING) * 2)
            .max(icon_size);
        let right = self
            .dimensions
            .pixel_width
            .saturating_sub(border.right.get() as usize)
            .saturating_sub(self.ui_px(WINDOW_TAB_INSET) + 2);
        let chrome = self.chrome();
        if self.library_push() > 0 && self.right_sidebar_width() == 0 {
            // Over the row the Library pushed along, which goes on under
            // them: on a ground of the row's own.
            let width = self.window_tab_window_buttons_width(button_size) + self.ui_px(WINDOW_TAB_INSET) + 2;
            self.filled_rectangle(
                layers,
                2,
                euclid::rect(
                    right.saturating_sub(width) as f32,
                    border.top.get() as f32,
                    (width + self.ui_px(WINDOW_TAB_INSET) + 2) as f32,
                    row_height as f32,
                ),
                self.tab_bar_surface(chrome.header_bg),
            )?;
        }
        let mut ui_items = Vec::new();
        self.paint_window_tab_window_buttons(
            layers,
            &mut ui_items,
            right,
            content_row_y,
            content_row_height,
            button_size,
            icon_size,
            chrome.text,
            chrome.secondary_text,
        )?;
        Ok(ui_items)
    }

    /// The window's own buttons at the end of the top-right pane bar, the
    /// right sidebar's toggle before them and a rule between them and the
    /// pane's buttons, while the pane bars stand in for a row that is away
    /// (see `pane_bar_window_chrome`). Returns where the pane's buttons go on
    /// from; `pane_bar_window_chrome_width` is the width this takes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_pane_bar_window_chrome(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        parts: PaneBarWindowChrome,
        bar_right: usize,
        bar_y: usize,
        bar_height: usize,
        button_size: usize,
        icon_size: usize,
    ) -> anyhow::Result<usize> {
        let chrome = self.chrome();
        let mut x = bar_right.saturating_sub(self.ui_px(WINDOW_TAB_INSET) + 2);
        if parts.window_buttons {
            // A right sidebar sliding away holds them until it is gone.
            x = if self.right_sidebar_holds_window_buttons() {
                x.saturating_sub(self.window_tab_window_buttons_width(button_size))
            } else {
                self.paint_window_tab_window_buttons(
                    layers,
                    ui_items,
                    x,
                    bar_y,
                    bar_height,
                    button_size,
                    icon_size,
                    chrome.text,
                    chrome.secondary_text,
                )?
            };
        }
        if parts.right_sidebar_toggle {
            x = x.saturating_sub(button_size);
            self.paint_window_tab_right_sidebar_toggle_button(
                layers,
                ui_items,
                x,
                bar_y,
                bar_height,
                button_size,
                icon_size,
                chrome.text,
                chrome.secondary_text,
            )?;
        }
        x = x.saturating_sub(self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP));
        let rule_height = button_size * 3 / 5;
        self.filled_rectangle(
            layers,
            1,
            euclid::rect(
                x as f32,
                (bar_y + bar_height.saturating_sub(rule_height) / 2) as f32,
                1.0,
                rule_height as f32,
            ),
            chrome.separator,
        )
        .context("pane bar window button rule")?;
        debug_assert_eq!(
            bar_right.saturating_sub(x),
            self.pane_bar_window_chrome_width(parts, button_size)
        );
        Ok(x)
    }

    /// What `paint_pane_bar_window_chrome` takes from the bar's right end.
    pub(crate) fn pane_bar_window_chrome_width(
        &self,
        parts: PaneBarWindowChrome,
        button_size: usize,
    ) -> usize {
        let gap = self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP);
        let window_buttons = if parts.window_buttons {
            self.window_tab_window_buttons_width(button_size)
        } else {
            0
        };
        let sidebar_toggle = if parts.right_sidebar_toggle {
            button_size
        } else {
            0
        };
        self.ui_px(WINDOW_TAB_INSET) + 2 + window_buttons + sidebar_toggle + gap
    }

    /// Paint only the essential native/client window chrome over a full-window
    /// ContentView. The normal tabs and sidebar actions stay suppressed, while
    /// the blank row retains window dragging and integrated buttons retain the
    /// exact same hit targets and visual treatment as the fancy tab bar.
    pub(crate) fn paint_full_window_chrome(
        &self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<Vec<UIItem>> {
        let row_height = self.full_window_client_chrome_height().ceil() as usize;
        if row_height == 0 {
            return Ok(Vec::new());
        }

        let border = self.get_os_border();
        let row_x = border.left.get() as usize;
        let row_y = border.top.get() as usize;
        let row_right = self
            .dimensions
            .pixel_width
            .saturating_sub(border.right.get() as usize);
        let row_width = row_right.saturating_sub(row_x);
        if row_width == 0 {
            return Ok(Vec::new());
        }

        let mut ui_items = vec![UIItem {
            x: row_x,
            y: row_y,
            width: row_width,
            height: row_height,
            item_type: UIItemType::TabBar(TabBarItem::None),
        }];

        if self.fancy_tab_bar_shows_window_buttons() {
            let chrome = self.chrome();
            let content_top_spacer = self.ui_px(WINDOW_TAB_TOP_SPACER).min(row_height);
            let content_row_y = row_y + content_top_spacer;
            let content_row_height = row_height.saturating_sub(content_top_spacer);
            let icon_size = self.ui_px(TAB_ICON_SIZE).min(content_row_height);
            let button_size = content_row_height
                .saturating_sub(self.ui_px(TAB_VERTICAL_PADDING) * 2)
                .max(icon_size);
            let action_right = row_right.saturating_sub(self.ui_px(WINDOW_TAB_INSET) + 2);
            self.paint_window_tab_window_buttons(
                layers,
                &mut ui_items,
                action_right,
                content_row_y,
                content_row_height,
                button_size,
                icon_size,
                chrome.text,
                chrome.secondary_text,
            )?;
        }

        Ok(ui_items)
    }

    pub(crate) fn fancy_tab_bar_shows_window_buttons(&self) -> bool {
        uses_integrated_window_buttons(self.config.window_decorations, self.window_state)
            && self.config.integrated_title_button_style != IntegratedTitleButtonStyle::MacOsNative
            && !self.config.integrated_title_buttons.is_empty()
    }

    /// Whether the window's buttons are the right sidebar's, at its top
    /// (see `paint_right_sidebar_window_buttons`): while it is docked, and
    /// while it slides back into the edge after, the row and the pane bars
    /// leaving them room where they go once it is gone.
    pub(crate) fn right_sidebar_holds_window_buttons(&self) -> bool {
        self.fancy_tab_bar_shows_window_buttons()
            && ((!self.right_sidebar_floats() && self.right_sidebar_docked_width() > 0)
                // The row's end is pushed past the window's edge, or on its
                // way back from there: they stay at its corner all the same.
                || self.library_drawn_push() > 0)
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_window_tab_leading_actions(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        row_x: usize,
        row_y: usize,
        row_height: usize,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
    ) -> anyhow::Result<()> {
        let action_slot_count = self.window_tab_leading_action_slot_count();
        if action_slot_count == 0 {
            return Ok(());
        }

        // The rounded-corner clearance is folded into
        // leading_action_start_pixels so layout reservation, painting and
        // hit-testing share the same origin.
        let mut button_x = row_x + self.window_tab_leading_action_start_pixels().ceil() as usize;
        for action_idx in 0..action_slot_count {
            let is_sidebar_toggle =
                action_idx == 0 && self.window_tab_shows_sidebar_toggle_action();
            let is_fullscreen_sidebar_toggle =
                is_sidebar_toggle && self.window_tab_sidebar_toggle_uses_fullscreen_style();
            let action_button_size = if is_sidebar_toggle {
                self.window_tab_sidebar_toggle_button_size()
            } else {
                self.ui_px(WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE)
            };
            let action_icon_size = if is_sidebar_toggle {
                self.window_tab_sidebar_toggle_icon_size()
            } else {
                self.ui_px(WINDOW_TAB_LEADING_ACTION_ICON_SIZE)
            };
            // action_button_size already comes from the shared
            // sidebar_toggle_size_px geometry (capsule height on non-mac
            // top fancy bars, fixed size otherwise); row_height here is
            // the content row (top spacer removed by the caller).
            let button_size = if is_fullscreen_sidebar_toggle {
                action_button_size
            } else {
                action_button_size
                    .min(row_height.saturating_sub(self.ui_px(4)))
                    .max(1)
            };
            let icon_size = action_icon_size.min(button_size.saturating_sub(2));

            if is_sidebar_toggle {
                let button_y = if is_fullscreen_sidebar_toggle {
                    row_y.saturating_sub(self.ui_px(WINDOW_TAB_TOP_SPACER))
                        + self.ui_px(SIDEBAR_INSET)
                        + self.ui_px(WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_Y_OFFSET)
                } else {
                    // Center like the window tab capsules (all platforms).
                    row_y + (row_height.saturating_sub(button_size) / 2)
                };
                self.paint_window_sidebar_toggle_button(
                    layers,
                    ui_items,
                    button_x,
                    button_y,
                    button_size,
                    icon_size,
                    foreground,
                    muted_fg,
                )?;
            }
            button_x += action_button_size + self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP);
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_window_sidebar_toggle_button(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        button_x: usize,
        button_y: usize,
        button_size: usize,
        icon_size: usize,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
    ) -> anyhow::Result<()> {
        let hovered = self.is_pointer_over_ui_rect(button_x, button_y, button_size, button_size);
        if hovered {
            self.fill_rounded_rectangle(
                layers,
                1,
                euclid::rect(
                    button_x as f32,
                    button_y as f32,
                    button_size as f32,
                    button_size as f32,
                ),
                foreground.mul_alpha(
                    if self.is_pointer_pressing_ui_rect(
                        button_x,
                        button_y,
                        button_size,
                        button_size,
                    ) {
                        0.20
                    } else {
                        0.12
                    },
                ),
                self.ui_px(SIDEBAR_INSET) as f32,
            )
            .context("window sidebar toggle hover")?;
        }

        ui_items.push(UIItem {
            x: button_x,
            y: button_y,
            width: button_size,
            height: button_size,
            item_type: UIItemType::WorkspaceSidebarToggle,
        });

        let icon_size = icon_size.min(button_size.saturating_sub(2)).max(1);
        self.paint_fancy_tab_icon(
            layers,
            self.workspace_sidebar_toggle_icon(),
            button_x + (button_size.saturating_sub(icon_size) / 2),
            button_y + (button_size.saturating_sub(icon_size) / 2),
            icon_size,
            if hovered { foreground } else { muted_fg },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_window_tab_window_buttons(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        mut action_right: usize,
        row_y: usize,
        row_height: usize,
        button_size: usize,
        icon_size: usize,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
    ) -> anyhow::Result<usize> {
        let gap = self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP) / 2;
        for button in self.config.integrated_title_buttons.iter().rev() {
            action_right = action_right.saturating_sub(button_size);
            self.paint_window_tab_window_button(
                layers,
                ui_items,
                *button,
                action_right,
                row_y,
                row_height,
                button_size,
                icon_size,
                foreground,
                muted_fg,
            )?;
            action_right = action_right.saturating_sub(gap);
        }

        Ok(action_right.saturating_sub(self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP) / 2))
    }

    /// What `paint_window_tab_window_buttons` takes from the right.
    fn window_tab_window_buttons_width(&self, button_size: usize) -> usize {
        let gap = self.ui_px(WINDOW_TAB_LEADING_ACTION_GAP);
        self.config.integrated_title_buttons.len() * (button_size + gap / 2) + gap / 2
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_window_tab_window_button(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        button: IntegratedTitleButton,
        button_x: usize,
        row_y: usize,
        row_height: usize,
        button_size: usize,
        icon_size: usize,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
    ) -> anyhow::Result<()> {
        let button_y = row_y + (row_height.saturating_sub(button_size) / 2);
        let hovered = self.is_pointer_over_ui_rect(button_x, button_y, button_size, button_size);
        let pressed = hovered
            && self.is_pointer_pressing_ui_rect(button_x, button_y, button_size, button_size);
        let press_inset = if pressed { 1 } else { 0 };
        let visual_size = button_size.saturating_sub(press_inset * 2);
        let close_button = button == IntegratedTitleButton::Close;

        if hovered {
            let chrome = self.chrome();
            let fill = if close_button {
                let mut red = LinearRgba::with_srgba(232, 17, 35, 255);
                if pressed {
                    red.3 = 0.82;
                }
                red
            } else if pressed {
                // Opaque, as the buttons beside them: a bordered fill is laid
                // over a disc of the border's colour, which showed through a
                // see-through tint and turned the button a dark grey disc.
                chrome.control_pressed_bg
            } else {
                chrome.control_hover_bg
            };
            let border = if close_button {
                LinearRgba::TRANSPARENT
            } else {
                foreground.mul_alpha(if pressed { 0.52 } else { 0.38 })
            };
            // On the trailing action mask's layer, after it, under the icon:
            // on a layer below, the mask hid every hover but the icon's, and
            // a hovered Close showed a white cross on a light bar.
            self.fill_rounded_rectangle_with_border(
                layers,
                2,
                euclid::rect(
                    (button_x + press_inset) as f32,
                    (button_y + press_inset) as f32,
                    visual_size as f32,
                    visual_size as f32,
                ),
                fill,
                border,
                WINDOW_TAB_ADD_BUTTON_RADIUS,
                ICON_BUTTON_BORDER_WIDTH,
            )
            .context("window tab title button hover")?;
        }

        ui_items.push(UIItem {
            x: button_x,
            y: button_y,
            width: button_size,
            height: button_size,
            item_type: UIItemType::TabBar(TabBarItem::WindowButton(button)),
        });

        let maximized = self
            .window_state
            .intersects(WindowState::MAXIMIZED | WindowState::FULL_SCREEN);
        let icon = match button {
            IntegratedTitleButton::Hide => SvgIcon::Minus,
            IntegratedTitleButton::Maximize if maximized => SvgIcon::Copy,
            IntegratedTitleButton::Maximize => SvgIcon::Square,
            IntegratedTitleButton::Close => SvgIcon::X,
        };
        let icon_size = if pressed {
            icon_size.saturating_sub(1).max(1)
        } else {
            icon_size
        };
        let icon_color = if close_button && hovered {
            LinearRgba(1.0, 1.0, 1.0, 1.0)
        } else if hovered {
            foreground
        } else {
            muted_fg
        };
        self.paint_fancy_tab_icon(
            layers,
            icon,
            button_x + press_inset + (visual_size.saturating_sub(icon_size) / 2),
            button_y + press_inset + (visual_size.saturating_sub(icon_size) / 2),
            icon_size,
            icon_color,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_window_tab(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        item: &TabEntry,
        tab_idx: usize,
        active: bool,
        is_renaming: bool,
        tab_left: f32,
        visible_left: f32,
        visible_width: f32,
        viewport_left: usize,
        viewport_right: usize,
        row_y: usize,
        row_height: usize,
        tab_width: usize,
        button_size: usize,
        icon_size: usize,
        font: &Rc<LoadedFont>,
        metrics: RenderMetrics,
        chrome: UiPalette,
        background: LinearRgba,
        foreground: LinearRgba,
    ) -> anyhow::Result<()> {
        let hover_x = visible_left.max(0.0) as usize;
        let hover_width = visible_width.max(0.0) as usize;
        let is_hovered = self.is_pointer_over_ui_rect(hover_x, row_y, hover_width, row_height);
        let tab_surface_color = if active {
            chrome.active_tab_surface()
        } else if is_hovered && !is_renaming {
            chrome.hover_fill(chrome.control_hover_bg)
        } else if self.chrome_see_through() {
            // The bar's own ground: a second coat would show as a darker pill.
            LinearRgba::TRANSPARENT
        } else {
            background
        };
        let tab_surface_y = row_y + (row_height.saturating_sub(button_size) / 2);

        ui_items.push(UIItem {
            x: visible_left.max(0.0) as usize,
            y: row_y,
            width: visible_width.max(0.0) as usize,
            height: row_height,
            item_type: UIItemType::TabBar(TabBarItem::Tab { tab_idx, active }),
        });

        // The whole tab -- backdrop, capsule, shadow and content -- is recorded
        // in the tab's own space, its left edge at 0, and replayed clipped to
        // the row's viewport.
        //
        // Recording it at full width is what keeps a cut tab looking like a cut
        // tab: `snapped_rounded_corner_radius` clamps the radius to half the
        // rect it is given, so drawing the capsule pre-cut turned a 30px sliver
        // into its own little lozenge instead of the end of a 320px pill. The
        // clip supplies the hard edge, so the corners need no special casing.
        //
        // Recording it in local space is what keeps the content in the right
        // place: once the row scrolls a tab's true origin goes negative, which
        // the unsigned coordinates the painters take cannot express, and laying
        // out from a clamped 0 drew the wrong slice and dropped the close
        // target on a neighbouring tab.
        let out_layers = layers;
        let mut content = HeapQuadAllocator::default();
        let mut content_layers = TripleLayerQuadAllocator::Heap(&mut content);
        let layers = &mut content_layers;

        // The bar's own ground under the tab. Over a see-through bar a second
        // coat of it would show as a darker block.
        if !self.chrome_see_through() {
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(0.0, row_y as f32, tab_width as f32, row_height as f32),
                background,
            )
            .context("window tab background")?;
        }

        self.paint_tab_capsule(
            layers,
            1,
            tab_surface_y,
            tab_width,
            button_size,
            self.ui_f32(WINDOW_TAB_RADIUS),
            tab_surface_color,
            if active {
                chrome.control_border
            } else {
                LinearRgba::TRANSPARENT
            },
            active,
        )
        .context("window tab surface")?;

        let icon_y = row_y + (row_height.saturating_sub(icon_size) / 2);
        // The terminal card's circle, as the pane tabs draw it; the tab's
        // status keeps its own place after it.
        let icon_right = self.paint_tab_circle_icon(
            layers,
            &crate::tab_icons::resolve_window_tab(),
            tab_surface_y,
            button_size,
            false,
            None,
        )?;

        let close_x = self.tab_close_button_x(tab_width, button_size, icon_size);
        let close_y = row_y + (row_height.saturating_sub(button_size) / 2);
        // Where the button really lands, for hover and hit testing. A target
        // placed from a clamped origin used to sit over the neighbouring tab
        // and close the wrong one.
        let close_screen_x = tab_left + close_x as f32;
        let close_hit_visible = close_screen_x >= viewport_left as f32
            && close_screen_x + button_size as f32 <= viewport_right as f32;
        let close_slot_reserved = self.config.show_close_tab_button_in_tabs && !is_renaming;
        let show_close = close_slot_reserved && (active || is_hovered);
        if show_close {
            // Hover feedback follows the hit target: a half-clipped button that
            // lit up without accepting the click sent it through to the tab
            // underneath, switching tabs instead of closing one.
            let close_hit_x = close_screen_x.max(0.0) as usize;
            let close_hovered = close_hit_visible
                && self.is_pointer_over_ui_rect(close_hit_x, close_y, button_size, button_size);
            if close_hovered {
                let hover_alpha = if self.is_pointer_pressing_ui_rect(
                    close_hit_x,
                    close_y,
                    button_size,
                    button_size,
                ) {
                    0.20
                } else {
                    0.12
                };
                let hover_inset = self.ui_px(TAB_CLOSE_HOVER_INSET).min(button_size / 2);
                let hover_size = button_size.saturating_sub(hover_inset * 2);
                self.fill_rounded_rectangle(
                    layers,
                    1,
                    euclid::rect(
                        (close_x + hover_inset) as f32,
                        (close_y + hover_inset) as f32,
                        hover_size as f32,
                        hover_size as f32,
                    ),
                    foreground.mul_alpha(hover_alpha),
                    TAB_CLOSE_HOVER_RADIUS,
                )
                .context("window tab close hover")?;
            }
            if close_hit_visible {
                ui_items.push(UIItem {
                    x: close_hit_x,
                    y: close_y,
                    width: button_size,
                    height: button_size,
                    item_type: UIItemType::CloseTab(tab_idx),
                });
            }
            let close_fg = if close_hovered {
                foreground
            } else {
                foreground.mul_alpha(0.82)
            };
            self.paint_fancy_tab_icon(
                layers,
                SvgIcon::X,
                close_x + (button_size.saturating_sub(icon_size) / 2),
                icon_y,
                icon_size,
                close_fg,
            )?;
        }

        let mut text_x = icon_right + self.ui_px(WINDOW_TAB_ICON_GAP);
        if let Some(status) = item.status {
            let status_x = text_x;
            self.paint_status_icon(layers, 2, status, status_x, icon_y, icon_size, foreground)
                .context("window tab status icon")?;
            text_x = status_x + icon_size + self.ui_px(WINDOW_TAB_ICON_GAP);
        }
        let text_right = if close_slot_reserved {
            close_x.saturating_sub(self.ui_px(WINDOW_TAB_ICON_GAP))
        } else {
            tab_width.saturating_sub(self.ui_px(TAB_CONTENT_INSET))
        };
        let text_width = text_right.saturating_sub(text_x);
        let text_y = row_y + (row_height.saturating_sub(metrics.cell_size.height as usize) / 2);
        if text_width > 0 {
            let text_fg = if is_renaming {
                self.filled_rectangle(
                    layers,
                    1,
                    euclid::rect(
                        text_x.saturating_sub(3) as f32,
                        text_y as f32,
                        text_width.saturating_add(6) as f32,
                        metrics.cell_size.height as f32,
                    ),
                    chrome.selected_bg,
                )
                .context("window tab rename selection")?;
                chrome.selected_text
            } else {
                foreground
            };
            self.paint_fancy_tab_text(
                layers,
                font,
                &item.title,
                item.status.is_some(),
                text_x,
                text_y,
                text_width,
                text_fg,
            )?;
        }

        drop(content_layers);
        content
            .apply_to_clipped_at(
                out_layers,
                tab_left,
                0.0,
                QuadClipRect::from_top_left_pixels(
                    viewport_left as f32,
                    row_y as f32,
                    viewport_right as f32,
                    (row_y + row_height) as f32,
                    &self.dimensions,
                ),
                1.0,
            )
            .context("window tab content")?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_content_view_tab(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        id: crate::termwindow::content_view::ContentViewId,
        title: &str,
        active: bool,
        tab_left: f32,
        visible_left: f32,
        visible_width: f32,
        viewport_left: usize,
        viewport_right: usize,
        row_y: usize,
        row_height: usize,
        tab_width: usize,
        button_size: usize,
        icon_size: usize,
        font: &Rc<LoadedFont>,
        metrics: RenderMetrics,
        chrome: UiPalette,
        background: LinearRgba,
        foreground: LinearRgba,
    ) -> anyhow::Result<()> {
        let is_hovered = self.is_pointer_over_ui_rect(
            visible_left.max(0.0) as usize,
            row_y,
            visible_width.max(0.0) as usize,
            row_height,
        );
        let surface = if active {
            chrome.active_tab_surface()
        } else if is_hovered {
            chrome.hover_fill(chrome.control_hover_bg)
        } else if self.chrome_see_through() {
            // As the window tabs: no second coat of the bar's ground.
            LinearRgba::TRANSPARENT
        } else {
            background
        };
        let tab_surface_y = row_y + (row_height.saturating_sub(button_size) / 2);

        ui_items.push(UIItem {
            x: visible_left.max(0.0) as usize,
            y: row_y,
            width: visible_width.max(0.0) as usize,
            height: row_height,
            item_type: UIItemType::TabBar(TabBarItem::ContentView { id }),
        });

        // Whole tab recorded at full width in the tab's own space, replayed
        // clipped to the row. See `paint_window_tab`.
        let out_layers = layers;
        let mut content = HeapQuadAllocator::default();
        let mut content_layers = TripleLayerQuadAllocator::Heap(&mut content);
        let layers = &mut content_layers;

        // As for a window tab: no second coat of a see-through bar.
        if !self.chrome_see_through() {
            self.filled_rectangle(
                layers,
                1,
                euclid::rect(0.0, row_y as f32, tab_width as f32, row_height as f32),
                background,
            )
            .context("content view tab background")?;
        }
        self.paint_tab_capsule(
            layers,
            1,
            tab_surface_y,
            tab_width,
            button_size,
            self.ui_f32(WINDOW_TAB_RADIUS),
            surface,
            if active {
                chrome.control_border
            } else {
                LinearRgba::TRANSPARENT
            },
            active,
        )
        .context("content view tab surface")?;

        let icon_x = self.ui_px(TAB_CONTENT_INSET);
        let icon_y = row_y + (row_height.saturating_sub(icon_size) / 2);
        self.paint_fancy_tab_icon(
            layers,
            SvgIcon::Link2,
            icon_x,
            icon_y,
            icon_size,
            foreground,
        )?;

        let close_x = self.tab_close_button_x(tab_width, button_size, icon_size);
        let close_y = row_y + (row_height.saturating_sub(button_size) / 2);
        let close_screen_x = tab_left + close_x as f32;
        let close_hit_visible = close_screen_x >= viewport_left as f32
            && close_screen_x + button_size as f32 <= viewport_right as f32;
        let show_close = active || is_hovered;
        if show_close {
            let close_hit_x = close_screen_x.max(0.0) as usize;
            let close_hovered = close_hit_visible
                && self.is_pointer_over_ui_rect(close_hit_x, close_y, button_size, button_size);
            if close_hit_visible {
                ui_items.push(UIItem {
                    x: close_hit_x,
                    y: close_y,
                    width: button_size,
                    height: button_size,
                    item_type: UIItemType::ContentViewClose(id),
                });
            }
            self.paint_fancy_tab_icon(
                layers,
                SvgIcon::X,
                close_x + (button_size.saturating_sub(icon_size) / 2),
                icon_y,
                icon_size,
                if close_hovered {
                    foreground
                } else {
                    foreground.mul_alpha(0.82)
                },
            )?;
        }

        let text_x = icon_x + icon_size + self.ui_px(WINDOW_TAB_ICON_GAP);
        let text_right = close_x.saturating_sub(self.ui_px(WINDOW_TAB_ICON_GAP));
        let text_width = text_right.saturating_sub(text_x);
        let text_y = row_y + (row_height.saturating_sub(metrics.cell_size.height as usize) / 2);
        if text_width > 0 {
            let line = Line::from_text(title, &CellAttributes::blank(), 0, None);
            self.paint_fancy_tab_text(
                layers, font, &line, false, text_x, text_y, text_width, foreground,
            )?;
        }

        drop(content_layers);
        content
            .apply_to_clipped_at(
                out_layers,
                tab_left,
                0.0,
                QuadClipRect::from_top_left_pixels(
                    viewport_left as f32,
                    row_y as f32,
                    viewport_right as f32,
                    (row_y + row_height) as f32,
                    &self.dimensions,
                ),
                1.0,
            )
            .context("content view tab content")?;
        Ok(())
    }

    /// A square icon button at the row's end: the new-tab button, the pin.
    /// `lit` draws the icon in the text's colour without the pointer on it.
    #[allow(clippy::too_many_arguments)]
    fn paint_window_tab_icon_button(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        button_x: usize,
        row_y: usize,
        row_height: usize,
        button_size: usize,
        icon_size: usize,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
        icon: SvgIcon,
        item_type: UIItemType,
        lit: bool,
    ) -> anyhow::Result<()> {
        let button_y = row_y + (row_height.saturating_sub(button_size) / 2);
        let hovered = self.is_pointer_over_ui_rect(button_x, button_y, button_size, button_size);
        let pressed = hovered
            && self.is_pointer_pressing_ui_rect(button_x, button_y, button_size, button_size);
        let press_inset = if pressed { 1 } else { 0 };
        let visual_size = button_size.saturating_sub(press_inset * 2);
        if hovered {
            let chrome = self.chrome();
            let fill = if pressed {
                chrome.control_pressed_bg
            } else {
                chrome.control_hover_bg
            };
            let border_alpha = if pressed { 0.52 } else { 0.38 };
            // Above the trailing action mask, as the window buttons'.
            self.fill_rounded_rectangle_with_border(
                layers,
                2,
                euclid::rect(
                    (button_x + press_inset) as f32,
                    (button_y + press_inset) as f32,
                    visual_size as f32,
                    visual_size as f32,
                ),
                fill,
                foreground.mul_alpha(border_alpha),
                WINDOW_TAB_ADD_BUTTON_RADIUS,
                ICON_BUTTON_BORDER_WIDTH,
            )
            .context("window tab button hover")?;
        }

        ui_items.push(UIItem {
            x: button_x,
            y: button_y,
            width: button_size,
            height: button_size,
            item_type,
        });

        let icon_size = if pressed {
            icon_size.saturating_sub(1).max(1)
        } else {
            icon_size
        };
        self.paint_fancy_tab_icon(
            layers,
            icon,
            button_x + press_inset + (visual_size.saturating_sub(icon_size) / 2),
            button_y + press_inset + (visual_size.saturating_sub(icon_size) / 2),
            icon_size,
            if hovered || lit { foreground } else { muted_fg },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_window_tab_right_sidebar_toggle_button(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        ui_items: &mut Vec<UIItem>,
        button_x: usize,
        row_y: usize,
        row_height: usize,
        button_size: usize,
        icon_size: usize,
        foreground: LinearRgba,
        muted_fg: LinearRgba,
    ) -> anyhow::Result<()> {
        // Every sidebar panel turned off: the button would open nothing.
        if !self.right_sidebar_has_panels() {
            return Ok(());
        }
        let button_y = row_y + (row_height.saturating_sub(button_size) / 2);
        let hovered = self.is_pointer_over_ui_rect(button_x, button_y, button_size, button_size);
        let pressed = hovered
            && self.is_pointer_pressing_ui_rect(button_x, button_y, button_size, button_size);
        let press_inset = if pressed { 1 } else { 0 };
        let visual_size = button_size.saturating_sub(press_inset * 2);
        if hovered {
            let chrome = self.chrome();
            let fill = if pressed {
                chrome.control_pressed_bg
            } else {
                chrome.control_hover_bg
            };
            let border_alpha = if pressed { 0.52 } else { 0.38 };
            // As the new-tab button beside it: above the trailing mask.
            self.fill_rounded_rectangle_with_border(
                layers,
                2,
                euclid::rect(
                    (button_x + press_inset) as f32,
                    (button_y + press_inset) as f32,
                    visual_size as f32,
                    visual_size as f32,
                ),
                fill,
                foreground.mul_alpha(border_alpha),
                WINDOW_TAB_ADD_BUTTON_RADIUS,
                ICON_BUTTON_BORDER_WIDTH,
            )
            .context("window tab right sidebar toggle hover")?;
        }

        ui_items.push(UIItem {
            x: button_x,
            y: button_y,
            width: button_size,
            height: button_size,
            item_type: UIItemType::RightSidebarToggle,
        });

        let icon_size = if pressed {
            icon_size.saturating_sub(1).max(1)
        } else {
            icon_size
        };
        self.paint_fancy_tab_icon(
            layers,
            self.right_sidebar_toggle_icon(),
            button_x + press_inset + (visual_size.saturating_sub(icon_size) / 2),
            button_y + press_inset + (visual_size.saturating_sub(icon_size) / 2),
            icon_size,
            if hovered { foreground } else { muted_fg },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_fancy_tab_text(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        title: &Line,
        strip_leading_progress: bool,
        x: usize,
        y: usize,
        width: usize,
        foreground: LinearRgba,
    ) -> anyhow::Result<()> {
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let cell_width = (metrics.cell_size.width as usize).max(1);
        let min_width = WINDOW_TAB_MIN_TEXT_COLS * cell_width;
        if width < min_width {
            return Ok(());
        }

        let mut text = String::new();
        let mut trimming_legacy_progress = strip_leading_progress;
        for cell in title.visible_cells() {
            let value = cell.str();
            if trimming_legacy_progress {
                if is_legacy_progress_marker(value) {
                    continue;
                }
                if text.is_empty() && value.trim().is_empty() {
                    continue;
                }
                trimming_legacy_progress = false;
            }
            text.push_str(value);
        }
        self.paint_fading_title(layers, font, &metrics, &text, x, y, width, foreground)
    }

    /// A tab's `title` in `width`. One too long goes out towards the end of
    /// the space it has, rather than stopping at "...".
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_fading_title(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        title: &str,
        x: usize,
        y: usize,
        width: usize,
        foreground: LinearRgba,
    ) -> anyhow::Result<()> {
        self.paint_title_fading_with(
            layers,
            font,
            metrics,
            title,
            x,
            y,
            width,
            foreground,
            TAB_TITLE_FADE_GAP,
            |fade| fade,
        )
    }

    /// `paint_fading_title`, ending `gap` short of `width`, with `fade_for`
    /// having the last word on the fade it is given, which is `None` for a
    /// title that fits.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_title_fading_with(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        title: &str,
        x: usize,
        y: usize,
        width: usize,
        foreground: LinearRgba,
        gap: f32,
        fade_for: impl FnOnce(Option<TextFade>) -> Option<TextFade>,
    ) -> anyhow::Result<()> {
        if width == 0 {
            return Ok(());
        }
        let (shaped, _) = self.cached_ui_shape(font, metrics, title)?;
        let advance: f32 = shaped
            .iter()
            .map(|info| info.glyph.x_advance.get() as f32)
            .sum();
        let right = (x + width) as f32;
        let fade = fade_for((advance > width as f32).then(|| {
            let to = right - self.ui_f32(gap).min(width as f32 / 4.0);
            TextFade {
                from: to - self.ui_f32(TITLE_FADE).min(width as f32 / 2.0),
                to,
                amount: 1.0,
            }
        }));
        // The glyph the end falls on is drawn too, for the fade to run out
        // across; those after it would not show.
        let slack = metrics.cell_size.width as f32 * 2.0;
        let clip_right = fade.map_or(right, |fade| right.max(fade.to + slack));
        self.paint_cached_ui_shape_fading(
            layers,
            metrics,
            &shaped,
            x as f32,
            y as f32,
            x as f32,
            clip_right,
            fade,
            |_| foreground,
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_ui_title_text(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        foreground: LinearRgba,
    ) -> anyhow::Result<()> {
        self.paint_ui_title_text_with_advance(layers, font, metrics, text, x, y, width, foreground)
            .map(|_| ())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_ui_title_text_with_advance(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        foreground: LinearRgba,
    ) -> anyhow::Result<f32> {
        if text.is_empty() || width == 0 {
            return Ok(0.0);
        }
        // Shape once via the shared cache and draw through the single emission
        // helper, so tab-bar/pane/sidebar title text stops re-shaping every frame
        // (it used to run harfbuzz inline on each paint).
        let (shaped, _) = self.cached_ui_shape(font, metrics, text)?;
        self.paint_cached_ui_shape(layers, metrics, &shaped, x, y, width, |_| foreground)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_ui_title_text_cached(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        foreground: LinearRgba,
    ) -> anyhow::Result<UiShapeCacheLookup> {
        self.paint_ui_title_text_cached_with_advance(
            layers, font, metrics, text, x, y, width, foreground,
        )
        .map(|(_, lookup)| lookup)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_ui_title_text_cached_with_advance(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        foreground: LinearRgba,
    ) -> anyhow::Result<(f32, UiShapeCacheLookup)> {
        if text.is_empty() || width == 0 {
            return Ok((0.0, UiShapeCacheLookup::Skipped));
        }

        let (shaped, lookup) = self.cached_ui_shape(font, metrics, text)?;
        let advance =
            self.paint_cached_ui_shape(layers, metrics, &shaped, x, y, width, |_| foreground)?;
        Ok((advance, lookup))
    }

    pub(crate) fn cached_ui_shape(
        &self,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
    ) -> anyhow::Result<(Rc<Vec<ShapedInfo>>, UiShapeCacheLookup)> {
        let font_identity = UI_SHAPE_CACHE_FONT_IDENTITY_BIT | font.id() as u64;
        let style = font.style();
        let key = BorrowedShapeCacheKey {
            font_identity,
            style,
            text,
        };
        let domain = self.ui_text_domain.get();

        match self
            .ui_shape_caches
            .borrow_mut()
            .domain_mut(domain)
            .get(&key as &dyn crate::shapecache::ShapeCacheKeyTrait)
        {
            Some(Ok(shaped)) => return Ok((Rc::clone(shaped), UiShapeCacheLookup::Hit)),
            Some(Err(err)) => return Err(anyhow!("cached shaper error: {}", err)),
            None => {}
        }

        let Some(window) = self.window.as_ref().cloned() else {
            return Ok((Rc::new(Vec::new()), UiShapeCacheLookup::Skipped));
        };
        let Some(gl_state) = self.render_state.as_ref() else {
            return Ok((Rc::new(Vec::new()), UiShapeCacheLookup::Skipped));
        };

        // Cache misses shape on the UI thread; surfaced per domain in Input
        // Diagnostics so fast-scroll shaping bursts are attributable.
        let miss_stage = crate::input_diagnostics::StageTimer::begin(domain.miss_stage_name());
        let infos = match font.shape(
            text,
            move |chars: &[char]| {
                window.notify(TermWindowNotif::InvalidateShapeCacheForChars(
                    chars.to_vec(),
                ))
            },
            crate::customglyph::filter_out_synthetic,
            None,
            Direction::LeftToRight,
            None,
            None,
        ) {
            Ok(infos) => infos,
            Err(err) => {
                if err.root_cause().downcast_ref::<ClearShapeCache>().is_some() {
                    miss_stage.finish(false);
                    return Err(err);
                }

                let res = anyhow!("shaper error: {}", err);
                self.ui_shape_caches
                    .borrow_mut()
                    .domain_mut(domain)
                    .put(key.to_owned(), Err(err));
                miss_stage.finish(false);
                return Err(res);
            }
        };

        let glyphs = {
            let mut glyph_cache = gl_state.glyph_cache.borrow_mut();
            self.glyph_infos_to_glyphs(style, &mut glyph_cache, &infos, font, metrics)?
        };
        let shaped = Rc::new(ShapedInfo::process(&infos, &glyphs));
        self.ui_shape_caches
            .borrow_mut()
            .domain_mut(domain)
            .put(key.to_owned(), Ok(Rc::clone(&shaped)));
        miss_stage.finish(true);
        Ok((shaped, UiShapeCacheLookup::Miss))
    }

    /// Total advance width (px) of `text` in `font`, via the shared shape cache.
    /// Reuses the same cached glyph run as the cached painters, so width queries
    /// (ellipsize, button sizing) become cache hits after the first shape and
    /// stay consistent with what is painted.
    pub(crate) fn cached_ui_text_advance(
        &self,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
    ) -> anyhow::Result<f32> {
        if text.is_empty() {
            return Ok(0.0);
        }
        let (shaped, _) = self.cached_ui_shape(font, metrics, text)?;
        Ok(shaped
            .iter()
            .map(|info| info.glyph.x_advance.get() as f32)
            .sum())
    }

    fn paint_cached_ui_shape<F>(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        metrics: &RenderMetrics,
        shaped: &[ShapedInfo],
        x: usize,
        y: usize,
        width: usize,
        color_for: F,
    ) -> anyhow::Result<f32>
    where
        F: FnMut(&ShapedInfo) -> LinearRgba,
    {
        self.paint_cached_ui_shape_clipped(
            layers,
            metrics,
            shaped,
            x as f32,
            y as f32,
            x as f32,
            (x + width) as f32,
            color_for,
        )
    }

    /// Draw a cached glyph run starting at pixel `start_x` (which may be left of
    /// `clip_left`, e.g. for a horizontally-scrolled line), clipping to
    /// `[clip_left, clip_right]`: glyphs fully left of `clip_left` advance the
    /// pen but draw nothing, and drawing stops at the first whole glyph whose
    /// advance does not fit before `clip_right` (see `glyph_fits`). The last
    /// glyph's ink may reach a pixel or two past it; a surface that must not
    /// be drawn past uses `paint_cached_ui_shape_pixel_clipped`. With
    /// `start_x == clip_left` this is identical to the previous fixed-origin
    /// painter, so existing callers are unaffected.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_cached_ui_shape_clipped<F>(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        metrics: &RenderMetrics,
        shaped: &[ShapedInfo],
        start_x: f32,
        y: f32,
        clip_left: f32,
        clip_right: f32,
        color_for: F,
    ) -> anyhow::Result<f32>
    where
        F: FnMut(&ShapedInfo) -> LinearRgba,
    {
        self.paint_cached_ui_shape_fading(
            layers, metrics, shaped, start_x, y, clip_left, clip_right, None, color_for,
        )
    }

    /// `paint_cached_ui_shape_clipped`, going out along `fade` if there is one.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_cached_ui_shape_fading<F>(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        metrics: &RenderMetrics,
        shaped: &[ShapedInfo],
        start_x: f32,
        y: f32,
        clip_left: f32,
        clip_right: f32,
        fade: Option<TextFade>,
        mut color_for: F,
    ) -> anyhow::Result<f32>
    where
        F: FnMut(&ShapedInfo) -> LinearRgba,
    {
        if shaped.is_empty() || clip_right <= clip_left {
            return Ok(0.0);
        }

        let Some(gl_state) = self.render_state.as_ref() else {
            return Ok(0.0);
        };
        let mut glyph_cache = gl_state.glyph_cache.borrow_mut();
        let left_offset = self.dimensions.pixel_width as f32 / -2.0;
        let top_offset = self.dimensions.pixel_height as f32 / -2.0;
        let baseline = metrics.cell_size.height as f32 + metrics.descender.get() as f32;
        let mut x_pos = start_x;

        for info in shaped {
            let advance = info.glyph.x_advance.get() as f32;
            // Fully left of the viewport: advance the pen, draw nothing.
            if x_pos + advance <= clip_left {
                x_pos += advance;
                continue;
            }
            let color = color_for(info);

            if let Some(key) = info.block_key {
                if !crate::ui::draw::glyph_fits(x_pos, advance, clip_right) {
                    break;
                }
                let sprite = glyph_cache.cached_block(key, metrics)?;
                let mut quad = layers.allocate(2)?;
                quad.set_position(
                    x_pos + left_offset,
                    y + top_offset,
                    x_pos + left_offset + advance,
                    y + top_offset + metrics.cell_size.height as f32,
                );
                quad.set_texture(sprite.texture_coords());
                set_glyph_color(&mut quad, color, x_pos, x_pos + advance, fade);
                quad.set_hsv(None);
                x_pos += advance;
                continue;
            }

            let glyph = &info.glyph;
            // By advance, so a label sized to its measured width keeps its
            // last glyph; see `glyph_fits`.
            if !crate::ui::draw::glyph_fits(x_pos, advance, clip_right) {
                break;
            }

            if let Some(texture) = glyph.texture.as_ref() {
                let glyph_x = x_pos + (glyph.x_offset + glyph.bearing_x).get() as f32;
                let glyph_y = y - (glyph.y_offset + glyph.bearing_y).get() as f32 + baseline;
                let glyph_width = texture.coords.size.width as f32 * glyph.scale as f32;
                let glyph_height = texture.coords.size.height as f32 * glyph.scale as f32;
                let mut quad = layers.allocate(2)?;
                quad.set_position(
                    glyph_x + left_offset,
                    glyph_y + top_offset,
                    glyph_x + left_offset + glyph_width,
                    glyph_y + top_offset + glyph_height,
                );
                quad.set_texture(texture.texture_coords());
                set_glyph_color(&mut quad, color, glyph_x, glyph_x + glyph_width, fade);
                quad.set_has_color(glyph.has_color);
                quad.set_hsv(None);
            }

            x_pos += advance;
        }

        Ok((x_pos - start_x).max(0.0))
    }

    /// Pixel-clip glyph quads and their texture coordinates at both horizontal
    /// edges. This is intended for horizontally scrollable text surfaces; fixed
    /// labels continue to use `paint_cached_ui_shape_clipped`, which preserves
    /// its whole-glyph truncation behavior.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_cached_ui_shape_pixel_clipped<F>(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        metrics: &RenderMetrics,
        shaped: &[ShapedInfo],
        start_x: f32,
        y: f32,
        clip_left: f32,
        clip_right: f32,
        color_for: F,
    ) -> anyhow::Result<f32>
    where
        F: FnMut(&ShapedInfo) -> LinearRgba,
    {
        self.paint_cached_ui_shape_pixel_clipped_scaled(
            layers, metrics, shaped, start_x, y, clip_left, clip_right, 1.0, color_for,
        )
    }

    /// `paint_cached_ui_shape_pixel_clipped` with every advance times
    /// `advance_scale`: a run spread out or drawn in to a width worked out
    /// without shaping it. The glyphs keep their size.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn paint_cached_ui_shape_pixel_clipped_scaled<F>(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        metrics: &RenderMetrics,
        shaped: &[ShapedInfo],
        start_x: f32,
        y: f32,
        clip_left: f32,
        clip_right: f32,
        advance_scale: f32,
        mut color_for: F,
    ) -> anyhow::Result<f32>
    where
        F: FnMut(&ShapedInfo) -> LinearRgba,
    {
        if shaped.is_empty() || clip_right <= clip_left {
            return Ok(0.0);
        }

        let Some(gl_state) = self.render_state.as_ref() else {
            return Ok(0.0);
        };
        let mut glyph_cache = gl_state.glyph_cache.borrow_mut();
        let left_offset = self.dimensions.pixel_width as f32 / -2.0;
        let top_offset = self.dimensions.pixel_height as f32 / -2.0;
        let baseline = metrics.cell_size.height as f32 + metrics.descender.get() as f32;
        let mut x_pos = start_x;

        for info in shaped {
            let advance = info.glyph.x_advance.get() as f32 * advance_scale;

            if let Some(key) = info.block_key {
                let sprite = glyph_cache.cached_block(key, metrics)?;
                let texture = sprite.texture_coords();
                if let Some(clip) = horizontal_texture_clip(
                    x_pos,
                    x_pos + advance,
                    texture.min_x(),
                    texture.max_x(),
                    clip_left,
                    clip_right,
                ) {
                    let color = color_for(info);
                    let mut quad = layers.allocate(2)?;
                    quad.set_position(
                        clip.position_left + left_offset,
                        y + top_offset,
                        clip.position_right + left_offset,
                        y + top_offset + metrics.cell_size.height as f32,
                    );
                    quad.set_texture_discrete(
                        clip.texture_left,
                        clip.texture_right,
                        texture.min_y(),
                        texture.max_y(),
                    );
                    quad.set_fg_color(color);
                    quad.set_alt_color_and_mix_value(color, 0.0);
                    quad.set_hsv(None);
                } else if x_pos >= clip_right {
                    break;
                }
                x_pos += advance;
                continue;
            }

            let glyph = &info.glyph;
            if let Some(texture) = glyph.texture.as_ref() {
                let glyph_x = x_pos + (glyph.x_offset + glyph.bearing_x).get() as f32;
                let glyph_y = y - (glyph.y_offset + glyph.bearing_y).get() as f32 + baseline;
                let glyph_width = texture.coords.size.width as f32 * glyph.scale as f32;
                let glyph_height = texture.coords.size.height as f32 * glyph.scale as f32;
                let texture_rect = texture.texture_coords();
                if let Some(clip) = horizontal_texture_clip(
                    glyph_x,
                    glyph_x + glyph_width,
                    texture_rect.min_x(),
                    texture_rect.max_x(),
                    clip_left,
                    clip_right,
                ) {
                    let color = color_for(info);
                    let mut quad = layers.allocate(2)?;
                    quad.set_position(
                        clip.position_left + left_offset,
                        glyph_y + top_offset,
                        clip.position_right + left_offset,
                        glyph_y + top_offset + glyph_height,
                    );
                    quad.set_texture_discrete(
                        clip.texture_left,
                        clip.texture_right,
                        texture_rect.min_y(),
                        texture_rect.max_y(),
                    );
                    quad.set_fg_color(color);
                    quad.set_alt_color_and_mix_value(color, 0.0);
                    quad.set_has_color(glyph.has_color);
                    quad.set_hsv(None);
                } else if glyph_x >= clip_right {
                    break;
                }
            } else if x_pos >= clip_right {
                break;
            }

            x_pos += advance;
        }

        Ok((x_pos - start_x).max(0.0))
    }

    fn paint_fancy_tab_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        icon: SvgIcon,
        x: usize,
        y: usize,
        size: usize,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        let gl_state = self.render_state.as_ref().unwrap();
        let sprite = gl_state
            .glyph_cache
            .borrow_mut()
            .cached_svg_icon(icon, size)?
            .texture_coords();

        let mut quad = layers.allocate(2)?;
        quad.set_position(
            x as f32 - left_offset,
            y as f32 - top_offset,
            x as f32 + size as f32 - left_offset,
            y as f32 + size as f32 - top_offset,
        );
        quad.set_texture(sprite);
        quad.set_fg_color(color);
        quad.set_alt_color_and_mix_value(color, 0.0);
        quad.set_hsv(None);
        quad.set_grayscale();

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct HorizontalTextureClip {
    position_left: f32,
    position_right: f32,
    texture_left: f32,
    texture_right: f32,
}

/// `color` on a glyph's quad, `fade` running across the span it covers.
fn set_glyph_color(
    quad: &mut impl QuadTrait,
    color: LinearRgba,
    left: f32,
    right: f32,
    fade: Option<TextFade>,
) {
    match fade {
        Some(fade) => {
            let left = color.mul_alpha(fade.alpha_at(left));
            let right = color.mul_alpha(fade.alpha_at(right));
            quad.set_corner_gradient(left, right, left, right);
        }
        None => quad.set_fg_color(color),
    }
    quad.set_alt_color_and_mix_value(color, 0.0);
}

fn horizontal_texture_clip(
    position_left: f32,
    position_right: f32,
    texture_left: f32,
    texture_right: f32,
    clip_left: f32,
    clip_right: f32,
) -> Option<HorizontalTextureClip> {
    let width = position_right - position_left;
    if !(width > 0.0 && texture_right > texture_left && clip_right > clip_left) {
        return None;
    }
    let visible_left = position_left.max(clip_left);
    let visible_right = position_right.min(clip_right);
    if visible_right <= visible_left {
        return None;
    }
    // A sub-pixel remnant of a clipped glyph (particularly the outer edge of
    // braces) rasterizes as a stray dot. Keep genuine partial glyphs, but drop
    // the final fragment once it is narrower than one physical pixel.
    let was_clipped = visible_left > position_left || visible_right < position_right;
    if was_clipped && visible_right - visible_left < 1.0 {
        return None;
    }
    let left_fraction = (visible_left - position_left) / width;
    let right_fraction = (visible_right - position_left) / width;
    let texture_width = texture_right - texture_left;
    Some(HorizontalTextureClip {
        position_left: visible_left,
        position_right: visible_right,
        texture_left: texture_left + texture_width * left_fraction,
        texture_right: texture_left + texture_width * right_fraction,
    })
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

#[cfg(test)]
mod pixel_clip_tests {
    use super::{horizontal_texture_clip, HorizontalTextureClip};

    #[test]
    fn horizontal_texture_clip_crops_position_and_uv_together() {
        let clip = horizontal_texture_clip(10.0, 30.0, 0.2, 0.6, 15.0, 25.0).unwrap();
        assert_eq!(
            clip,
            HorizontalTextureClip {
                position_left: 15.0,
                position_right: 25.0,
                texture_left: 0.3,
                texture_right: 0.5,
            }
        );
    }

    #[test]
    fn horizontal_texture_clip_rejects_invisible_or_invalid_quads() {
        assert!(horizontal_texture_clip(10.0, 20.0, 0.2, 0.6, 20.0, 30.0).is_none());
        assert!(horizontal_texture_clip(10.0, 10.0, 0.2, 0.6, 0.0, 30.0).is_none());
        assert!(horizontal_texture_clip(10.0, 20.0, 0.6, 0.2, 0.0, 30.0).is_none());
    }

    #[test]
    fn horizontal_texture_clip_hides_subpixel_clipped_remnants() {
        assert!(horizontal_texture_clip(10.0, 20.0, 0.2, 0.6, 19.25, 30.0).is_none());

        // Do not discard a naturally narrow quad when no clipping occurred.
        assert!(horizontal_texture_clip(10.0, 10.75, 0.2, 0.6, 0.0, 30.0).is_some());
    }
}
