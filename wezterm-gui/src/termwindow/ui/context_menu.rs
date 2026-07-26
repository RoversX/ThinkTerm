use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::{UIItem, UIItemType};
use crate::ui::UiPalette;
use crate::utilsprites::RenderMetrics;
use ::window::color::LinearRgba;
use ::window::{
    Appearance, ContextMenuAction, ContextMenuIcon, ContextMenuItem, MouseCursor, MouseEvent,
    MouseEventKind, MousePress, WindowOps,
};
use anyhow::Context;
use mux::pane::Pane;
use std::rc::Rc;
use std::sync::Arc;
use wezterm_font::LoadedFont;

// Design pixels (2x macOS backing), scaled via ui_px/ui_f32 like the rest
// of the chrome. The fallback menu only shows on non-mac (and behind a
// debug flag on mac).
const MENU_MIN_WIDTH: usize = 400;
const MENU_MAX_WIDTH: usize = 1000;
const MENU_WINDOW_MARGIN: usize = 24;
const MENU_PADDING_X: usize = 22;
const MENU_PADDING_Y: usize = 12;
const MENU_LABEL_GAP: usize = 16;
const MENU_ICON_SIZE: usize = 32;
const MENU_ICON_SLOT: usize = 48;
const MENU_CHECK_SLOT: usize = 40;
const MENU_ARROW_SLOT: usize = 44;
const MENU_ROW_EXTRA_HEIGHT: usize = 24;
const MENU_SEPARATOR_HEIGHT: usize = 14;
const MENU_BORDER_WIDTH: f32 = 2.0;
const MENU_RADIUS: f32 = 28.0;
const MENU_ROW_RADIUS: f32 = 18.0;
const MENU_ROW_HOVER_INSET_X: usize = 10;
const MENU_ROW_HOVER_INSET_Y: usize = 4;

pub(crate) fn reveal_in_folder_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Reveal in Finder"
    } else if cfg!(target_os = "windows") {
        "Show in File Explorer"
    } else {
        "Show in Folder"
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ContextMenuState {
    anchor: window::Point,
    items: Vec<ContextMenuItem>,
    active_path: Option<Vec<usize>>,
    layout: Vec<ContextMenuLayoutItem>,
}

#[derive(Clone, Debug)]
struct ContextMenuLayoutItem {
    path: Vec<usize>,
    rect: MenuRect,
}

#[derive(Clone, Copy, Debug)]
struct MenuRect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

#[derive(Clone, Copy)]
enum MenuRow<'a> {
    Item {
        index: usize,
        item: &'a ContextMenuItem,
    },
    Separator,
}

#[derive(Clone, Copy)]
struct MenuMetrics {
    width: usize,
    height: usize,
    row_height: usize,
}

impl ContextMenuState {
    pub(crate) fn new(anchor: window::Point, items: Vec<ContextMenuItem>) -> Self {
        Self {
            anchor,
            items,
            active_path: None,
            layout: Vec::new(),
        }
    }

    pub(crate) fn has_renderable_items(items: &[ContextMenuItem]) -> bool {
        items
            .iter()
            .any(|item| matches!(item, ContextMenuItem::Item { .. }))
    }

    fn set_active_path(&mut self, path: Option<Vec<usize>>) -> bool {
        if self.active_path == path {
            return false;
        }
        self.active_path = path;
        true
    }

    fn hit_path(&self, coords: window::Point) -> Option<Vec<usize>> {
        self.layout
            .iter()
            .rev()
            .find(|item| item.rect.hit_test(coords.x, coords.y))
            .map(|item| item.path.clone())
    }

    fn item_at_path(&self, path: &[usize]) -> Option<&ContextMenuItem> {
        item_at_path(&self.items, path)
    }

    fn path_has_submenu(&self, path: &[usize]) -> bool {
        match self.item_at_path(path) {
            Some(ContextMenuItem::Item {
                submenu, enabled, ..
            }) => *enabled && Self::has_renderable_items(submenu),
            _ => false,
        }
    }

    fn action_for_path(&self, path: &[usize]) -> Option<ContextMenuAction> {
        match self.item_at_path(path) {
            Some(ContextMenuItem::Item {
                action,
                enabled,
                submenu,
                ..
            }) if *enabled && !Self::has_renderable_items(submenu) => Some(action.clone()),
            _ => None,
        }
    }

    fn path_is_enabled(&self, path: &[usize]) -> bool {
        match self.item_at_path(path) {
            Some(ContextMenuItem::Item { enabled, .. }) => *enabled,
            _ => false,
        }
    }
}

impl MenuRect {
    fn hit_test(self, x: isize, y: isize) -> bool {
        x >= self.x as isize
            && x < self.x.saturating_add(self.width) as isize
            && y >= self.y as isize
            && y < self.y.saturating_add(self.height) as isize
    }
}

impl crate::TermWindow {
    pub(crate) fn begin_context_menu_application_actions(&mut self) {
        self.context_menu_application_actions.clear();
    }

    pub(crate) fn context_menu_application_item_with_icon(
        &mut self,
        label: impl Into<String>,
        icon: ContextMenuIcon,
        action: crate::termwindow::ContextMenuApplicationAction,
        enabled: bool,
    ) -> ContextMenuItem {
        let action_id = self.next_context_menu_application_action_id;
        self.next_context_menu_application_action_id = self
            .next_context_menu_application_action_id
            .wrapping_add(1)
            .max(1);
        self.context_menu_application_actions
            .insert(action_id, action);
        let item = ContextMenuItem::application_item(label, action_id).with_icon(icon);
        if enabled {
            item
        } else {
            item.disabled()
        }
    }

    pub(crate) fn perform_context_menu_application_action(&mut self, action_id: u64) {
        let Some(action) = self
            .context_menu_application_actions
            .get(&action_id)
            .cloned()
        else {
            return;
        };
        match action {
            crate::termwindow::ContextMenuApplicationAction::Note(command) => {
                self.perform_right_sidebar_note_command(command);
            }
            crate::termwindow::ContextMenuApplicationAction::ActivateWorkspaceThread {
                space_id,
                thread_id,
            } => {
                let Some(window) = self.window.clone() else {
                    return;
                };
                // One activation of exactly the notified thread; letting
                // switch_space start the Space's recorded thread first can
                // land on the wrong thread when activation is asynchronous.
                let navigated =
                    self.switch_space_to_thread(space_id, Some(thread_id.clone()), &window);
                // A Space owned by another window cannot be navigated from
                // here; keep the notification so the click is not lost.
                if navigated
                    && crate::workspace_threads::acknowledge_thread_work_for_thread(&thread_id)
                {
                    self.invalidate_window();
                }
            }
            crate::termwindow::ContextMenuApplicationAction::ToggleWorkspaceStatusFilter(
                status,
            ) => {
                self.toggle_workspace_sidebar_status_filter(status);
            }
        }
    }

    pub(crate) fn show_term_context_menu(
        &mut self,
        context: &dyn WindowOps,
        coords: window::Point,
        items: Vec<ContextMenuItem>,
    ) {
        self.close_fallback_context_menu();
        if !ContextMenuState::has_renderable_items(&items) {
            return;
        }

        if cfg!(target_os = "macos") && !crate::native_settings::force_fallback_context_menu() {
            context.show_context_menu(coords, items);
            return;
        }

        self.context_menu = Some(ContextMenuState::new(coords, items));
        context.invalidate();
    }

    pub(crate) fn mouse_event_context_menu(
        &mut self,
        event: &MouseEvent,
        pane: &Arc<dyn Pane>,
        context: &dyn WindowOps,
    ) -> bool {
        let Some(menu) = self.context_menu.as_mut() else {
            return false;
        };

        match event.kind {
            MouseEventKind::Move => {
                let path = menu.hit_path(event.coords);
                let cursor = path
                    .as_deref()
                    .filter(|path| menu.path_is_enabled(path))
                    .map(|_| MouseCursor::Hand)
                    .unwrap_or(MouseCursor::Arrow);
                context.set_cursor(Some(cursor));
                if menu.set_active_path(path) {
                    context.invalidate();
                }
                true
            }
            MouseEventKind::Press(MousePress::Left) => {
                self.context_menu_suppressed_release = Some(MousePress::Left);
                let path = menu.hit_path(event.coords);
                if let Some(path) = path {
                    if menu.path_has_submenu(&path) {
                        if menu.set_active_path(Some(path)) {
                            context.invalidate();
                        }
                        return true;
                    }

                    if let Some(action) = menu.action_for_path(&path) {
                        self.close_fallback_context_menu();
                        context.invalidate();
                        match action {
                            ContextMenuAction::KeyAssignment(action) => {
                                if let Err(err) = self.perform_key_assignment(pane, &action) {
                                    log::error!("context menu action failed: {err:#}");
                                }
                            }
                            ContextMenuAction::ApplicationAction(action_id) => {
                                self.perform_context_menu_application_action(action_id);
                            }
                        }
                        return true;
                    }

                    if menu.set_active_path(Some(path)) {
                        context.invalidate();
                    }
                    return true;
                }

                self.close_fallback_context_menu();
                context.invalidate();
                true
            }
            MouseEventKind::Press(MousePress::Right) => {
                if menu.hit_path(event.coords).is_some() {
                    return true;
                }
                self.close_fallback_context_menu();
                context.invalidate();
                false
            }
            MouseEventKind::Release(_)
            | MouseEventKind::VertWheel(_)
            | MouseEventKind::HorzWheel(_) => true,
            MouseEventKind::Press(MousePress::Middle) => true,
        }
    }

    pub(crate) fn consume_context_menu_suppressed_release(&mut self, event: &MouseEvent) -> bool {
        let MouseEventKind::Release(press) = event.kind else {
            return false;
        };
        if self.context_menu_suppressed_release == Some(press) {
            self.context_menu_suppressed_release = None;
            return true;
        }
        false
    }

    pub(crate) fn close_fallback_context_menu(&mut self) {
        if self.context_menu.is_none() {
            return;
        }

        self.context_menu = None;
        self.last_ui_item = None;
        self.ui_items.retain(|item| {
            !matches!(
                item.item_type,
                UIItemType::ContextMenuItem(_) | UIItemType::ContextMenuBackdrop
            )
        });
    }

    pub(crate) fn paint_context_menu(&mut self) -> anyhow::Result<()> {
        let Some(menu) = self.context_menu.as_ref() else {
            return Ok(());
        };

        let anchor = menu.anchor;
        let items = menu.items.clone();
        let active_path = menu.active_path.clone();

        let native_settings = crate::native_settings::load();
        let menu_font_size = crate::native_settings::home_font_size(&native_settings);
        let ui_font = self
            .fonts
            .title_font_with_size(menu_font_size)
            .context("context menu ui font")?;
        let ui_metrics = RenderMetrics::with_font_metrics(&ui_font.metrics());
        let row_height = (ui_metrics.cell_size.height as usize + self.ui_px(MENU_ROW_EXTRA_HEIGHT))
            .max(self.ui_px(44));
        let palette = context_menu_palette(crate::native_settings::effective_appearance());

        let gl_state = self.render_state.as_ref().unwrap();
        let layer = gl_state.layer_for_zindex(0).context("context menu layer")?;
        let mut layers = layer.quad_allocator();

        self.ui_items.push(UIItem {
            x: 0,
            y: 0,
            width: self.dimensions.pixel_width,
            height: self.dimensions.pixel_height,
            item_type: UIItemType::ContextMenuBackdrop,
        });

        let mut layout: Vec<ContextMenuLayoutItem> = Vec::new();
        let mut menu_paths = vec![Vec::new()];
        if let Some(active_path) = active_path.as_ref() {
            let mut prefix = Vec::new();
            for idx in active_path {
                prefix.push(*idx);
                if item_at_path(&items, &prefix)
                    .and_then(renderable_submenu)
                    .is_some()
                {
                    menu_paths.push(prefix.clone());
                }
            }
        }

        for menu_path in menu_paths {
            let Some(menu_items) = items_for_menu_path(&items, &menu_path) else {
                continue;
            };
            let rows = render_rows(menu_items);
            if rows.is_empty() {
                continue;
            }

            let metrics = compute_menu_metrics(
                self,
                &ui_font,
                row_height,
                &rows,
                self.dimensions.pixel_width,
            )?;
            let origin = if menu_path.is_empty() {
                clamp_menu_origin(
                    anchor.x,
                    anchor.y,
                    metrics.width,
                    metrics.height,
                    self.dimensions.pixel_width,
                    self.dimensions.pixel_height,
                )
            } else {
                let parent_rect = layout
                    .iter()
                    .find(|item| item.path == menu_path)
                    .map(|item| item.rect);
                let Some(parent_rect) = parent_rect else {
                    continue;
                };
                submenu_origin(
                    parent_rect,
                    metrics.width,
                    metrics.height,
                    self.dimensions.pixel_width,
                    self.dimensions.pixel_height,
                )
            };

            paint_menu_panel(self, &mut layers, palette, origin, metrics)?;
            paint_menu_rows(
                self,
                &mut layers,
                &ui_font,
                ui_metrics,
                palette,
                &menu_path,
                &rows,
                origin,
                metrics,
                active_path.as_deref(),
                &mut layout,
            )?;
        }

        drop(layers);
        if let Some(menu) = self.context_menu.as_mut() {
            menu.layout = layout;
        }

        Ok(())
    }
}

fn item_at_path<'a>(items: &'a [ContextMenuItem], path: &[usize]) -> Option<&'a ContextMenuItem> {
    let mut current = items;
    let mut item = None;
    for idx in path {
        item = current.get(*idx);
        current = match item {
            Some(ContextMenuItem::Item { submenu, .. }) => submenu,
            _ => return item,
        };
    }
    item
}

fn items_for_menu_path<'a>(
    items: &'a [ContextMenuItem],
    path: &[usize],
) -> Option<&'a [ContextMenuItem]> {
    if path.is_empty() {
        return Some(items);
    }
    item_at_path(items, path).and_then(renderable_submenu)
}

fn renderable_submenu(item: &ContextMenuItem) -> Option<&[ContextMenuItem]> {
    match item {
        ContextMenuItem::Item {
            submenu, enabled, ..
        } if *enabled && ContextMenuState::has_renderable_items(submenu) => Some(submenu),
        _ => None,
    }
}

fn render_rows(items: &[ContextMenuItem]) -> Vec<MenuRow<'_>> {
    let mut rows = Vec::new();
    let mut last_was_separator = true;

    for (index, item) in items.iter().enumerate() {
        match item {
            ContextMenuItem::Item { .. } => {
                rows.push(MenuRow::Item { index, item });
                last_was_separator = false;
            }
            ContextMenuItem::Separator => {
                if !last_was_separator {
                    rows.push(MenuRow::Separator);
                    last_was_separator = true;
                }
            }
        }
    }

    while matches!(rows.last(), Some(MenuRow::Separator)) {
        rows.pop();
    }

    rows
}

fn compute_menu_metrics(
    term: &crate::TermWindow,
    font: &Rc<LoadedFont>,
    row_height: usize,
    rows: &[MenuRow<'_>],
    window_width: usize,
) -> anyhow::Result<MenuMetrics> {
    let mut label_width = 0usize;
    let mut height = term.ui_px(MENU_PADDING_Y) * 2;

    for row in rows {
        match row {
            MenuRow::Item { item, .. } => {
                if let ContextMenuItem::Item { label, .. } = item {
                    label_width =
                        label_width.max(term.sidebar_text_width(font, label)?.ceil() as usize);
                }
                height = height.saturating_add(row_height);
            }
            MenuRow::Separator => {
                height = height.saturating_add(term.ui_px(MENU_SEPARATOR_HEIGHT));
            }
        }
    }

    let ideal_width = term.ui_px(MENU_PADDING_X) * 2
        + term.ui_px(MENU_CHECK_SLOT)
        + term.ui_px(MENU_ICON_SLOT)
        + term.ui_px(MENU_LABEL_GAP)
        + label_width
        + term.ui_px(MENU_ARROW_SLOT);
    let max_width = term
        .ui_px(MENU_MAX_WIDTH)
        .min(window_width.saturating_sub(term.ui_px(MENU_WINDOW_MARGIN) * 2))
        .max(1);
    let min_width = term.ui_px(MENU_MIN_WIDTH).min(max_width);
    let width = ideal_width.clamp(min_width, max_width);

    Ok(MenuMetrics {
        width,
        height,
        row_height,
    })
}

fn clamp_menu_origin(
    x: isize,
    y: isize,
    width: usize,
    height: usize,
    window_width: usize,
    window_height: usize,
) -> (usize, usize) {
    let max_x = window_width.saturating_sub(width) as isize;
    let max_y = window_height.saturating_sub(height) as isize;
    (
        x.clamp(0, max_x).max(0) as usize,
        y.clamp(0, max_y).max(0) as usize,
    )
}

fn submenu_origin(
    parent: MenuRect,
    width: usize,
    height: usize,
    window_width: usize,
    window_height: usize,
) -> (usize, usize) {
    let right_x = parent.x.saturating_add(parent.width).saturating_sub(4);
    let left_x = parent.x.saturating_sub(width.saturating_sub(4));
    let x = if right_x.saturating_add(width) <= window_width {
        right_x
    } else {
        left_x
    };
    let y = parent.y.min(window_height.saturating_sub(height)).max(0);
    (x, y)
}

fn paint_menu_panel(
    term: &crate::TermWindow,
    layers: &mut TripleLayerQuadAllocator<'_>,
    palette: UiPalette,
    origin: (usize, usize),
    metrics: MenuMetrics,
) -> anyhow::Result<()> {
    let (x, y) = origin;
    let rect = euclid::rect(
        x as f32,
        y as f32,
        metrics.width as f32,
        metrics.height as f32,
    );
    term.fill_rounded_rectangle(
        layers,
        2,
        euclid::rect(
            x.saturating_add(4) as f32,
            y.saturating_add(5) as f32,
            metrics.width as f32,
            metrics.height as f32,
        ),
        LinearRgba::with_components(0.0, 0.0, 0.0, 0.18),
        term.ui_f32(MENU_RADIUS),
    )?;
    term.fill_rounded_rectangle_with_border(
        layers,
        2,
        rect,
        palette.control_bg,
        palette.control_border,
        term.ui_f32(MENU_RADIUS),
        term.ui_f32(MENU_BORDER_WIDTH).max(1.0),
    )
}

#[allow(clippy::too_many_arguments)]
fn paint_menu_rows(
    term: &mut crate::TermWindow,
    layers: &mut TripleLayerQuadAllocator<'_>,
    font: &Rc<LoadedFont>,
    font_metrics: RenderMetrics,
    palette: UiPalette,
    menu_path: &[usize],
    rows: &[MenuRow<'_>],
    origin: (usize, usize),
    metrics: MenuMetrics,
    active_path: Option<&[usize]>,
    layout: &mut Vec<ContextMenuLayoutItem>,
) -> anyhow::Result<()> {
    let (x, y) = origin;
    let mut cursor_y = y + term.ui_px(MENU_PADDING_Y);

    for row in rows {
        match row {
            MenuRow::Separator => {
                let sep_y = cursor_y + term.ui_px(MENU_SEPARATOR_HEIGHT) / 2;
                term.filled_rectangle(
                    layers,
                    2,
                    euclid::rect(
                        (x + term.ui_px(MENU_PADDING_X)) as f32,
                        sep_y as f32,
                        metrics.width.saturating_sub(term.ui_px(MENU_PADDING_X) * 2) as f32,
                        1.0,
                    ),
                    palette.separator,
                )?;
                cursor_y += term.ui_px(MENU_SEPARATOR_HEIGHT);
            }
            MenuRow::Item { index, item } => {
                let mut path = menu_path.to_vec();
                path.push(*index);
                let item_rect = MenuRect {
                    x: x + term.ui_f32(MENU_BORDER_WIDTH) as usize,
                    y: cursor_y,
                    width: metrics
                        .width
                        .saturating_sub((term.ui_f32(MENU_BORDER_WIDTH) as usize) * 2),
                    height: metrics.row_height,
                };
                let has_submenu = match item {
                    ContextMenuItem::Item { submenu, .. } => {
                        ContextMenuState::has_renderable_items(submenu)
                    }
                    ContextMenuItem::Separator => false,
                };
                let hovered = active_path.is_some_and(|active| {
                    active == path.as_slice() || (has_submenu && active.starts_with(&path))
                });

                if hovered {
                    term.fill_rounded_rectangle(
                        layers,
                        2,
                        euclid::rect(
                            item_rect
                                .x
                                .saturating_add(term.ui_px(MENU_ROW_HOVER_INSET_X))
                                as f32,
                            item_rect
                                .y
                                .saturating_add(term.ui_px(MENU_ROW_HOVER_INSET_Y))
                                as f32,
                            item_rect
                                .width
                                .saturating_sub(term.ui_px(MENU_ROW_HOVER_INSET_X) * 2)
                                as f32,
                            item_rect
                                .height
                                .saturating_sub(term.ui_px(MENU_ROW_HOVER_INSET_Y) * 2)
                                as f32,
                        ),
                        palette.control_hover_bg,
                        term.ui_f32(MENU_ROW_RADIUS),
                    )?;
                }

                if let ContextMenuItem::Item {
                    label,
                    icon,
                    checked,
                    enabled,
                    submenu,
                    ..
                } = item
                {
                    let foreground = if !enabled {
                        palette.muted_text
                    } else if hovered {
                        palette.text
                    } else {
                        palette.secondary_text
                    };
                    let icon_y = item_rect.y
                        + (item_rect.height.saturating_sub(term.ui_px(MENU_ICON_SIZE))) / 2;
                    let check_x = item_rect.x + term.ui_px(MENU_PADDING_X);
                    let icon_x = check_x + term.ui_px(MENU_CHECK_SLOT);
                    if *checked {
                        term.paint_sidebar_icon(
                            layers,
                            SvgIcon::Check,
                            check_x,
                            icon_y,
                            term.ui_px(MENU_ICON_SIZE),
                            foreground,
                        )?;
                    }
                    if let Some(icon) = icon.as_ref().and_then(|icon| menu_icon(icon)) {
                        term.paint_sidebar_icon(
                            layers,
                            icon,
                            icon_x,
                            icon_y,
                            term.ui_px(MENU_ICON_SIZE),
                            foreground,
                        )?;
                    }

                    let label_x = item_rect.x
                        + term.ui_px(MENU_PADDING_X)
                        + term.ui_px(MENU_CHECK_SLOT)
                        + term.ui_px(MENU_ICON_SLOT)
                        + term.ui_px(MENU_LABEL_GAP);
                    let arrow_width = if ContextMenuState::has_renderable_items(submenu) {
                        term.ui_px(MENU_ARROW_SLOT)
                    } else {
                        0
                    };
                    let label_width = item_rect
                        .width
                        .saturating_sub(label_x.saturating_sub(item_rect.x))
                        .saturating_sub(arrow_width)
                        .saturating_sub(term.ui_px(MENU_PADDING_X));
                    let text_y = item_rect.y
                        + (item_rect
                            .height
                            .saturating_sub(font_metrics.cell_size.height as usize))
                            / 2;
                    term.paint_sidebar_text(
                        layers,
                        font,
                        font_metrics,
                        label,
                        label_x,
                        text_y,
                        label_width,
                        foreground,
                    )?;

                    if ContextMenuState::has_renderable_items(submenu) {
                        let arrow_x = item_rect
                            .x
                            .saturating_add(item_rect.width)
                            .saturating_sub(term.ui_px(MENU_PADDING_X))
                            .saturating_sub(term.ui_px(MENU_ICON_SIZE));
                        term.paint_sidebar_icon(
                            layers,
                            SvgIcon::ChevronRight,
                            arrow_x,
                            icon_y,
                            term.ui_px(MENU_ICON_SIZE),
                            foreground,
                        )?;
                    }
                }

                term.ui_items.push(UIItem {
                    x: item_rect.x,
                    y: item_rect.y,
                    width: item_rect.width,
                    height: item_rect.height,
                    item_type: UIItemType::ContextMenuItem(path.clone()),
                });
                layout.push(ContextMenuLayoutItem {
                    path,
                    rect: item_rect,
                });
                cursor_y += metrics.row_height;
            }
        }
    }

    Ok(())
}

fn context_menu_palette(appearance: Appearance) -> UiPalette {
    let mut palette = UiPalette::for_appearance(appearance);
    match appearance {
        Appearance::Dark | Appearance::DarkHighContrast => {
            palette.control_bg = LinearRgba::with_srgba(30, 30, 32, 255);
            palette.control_hover_bg = LinearRgba::with_srgba(255, 255, 255, 255).mul_alpha(0.08);
            palette.control_border = LinearRgba::with_srgba(118, 118, 128, 255).mul_alpha(0.34);
            palette.separator = LinearRgba::with_srgba(84, 84, 88, 255).mul_alpha(0.36);
            palette.text = LinearRgba::with_srgba(242, 242, 247, 255);
            palette.secondary_text = LinearRgba::with_srgba(226, 226, 232, 255);
            palette.muted_text = LinearRgba::with_srgba(150, 150, 156, 255);
        }
        Appearance::Light | Appearance::LightHighContrast => {
            palette.control_bg = LinearRgba::with_srgba(246, 246, 248, 255);
            palette.control_hover_bg = LinearRgba::with_srgba(60, 60, 67, 255).mul_alpha(0.08);
            palette.control_border = LinearRgba::with_srgba(60, 60, 67, 255).mul_alpha(0.22);
            palette.separator = LinearRgba::with_srgba(60, 60, 67, 255).mul_alpha(0.20);
        }
    }
    palette
}

fn menu_icon(icon: &ContextMenuIcon) -> Option<SvgIcon> {
    match icon {
        ContextMenuIcon::Application | ContextMenuIcon::ExternalLink => Some(SvgIcon::ExternalLink),
        ContextMenuIcon::Back | ContextMenuIcon::MoveLeft => Some(SvgIcon::ArrowLeft),
        ContextMenuIcon::Check => Some(SvgIcon::CircleCheck),
        ContextMenuIcon::Close => Some(SvgIcon::X),
        ContextMenuIcon::Code => Some(SvgIcon::CodeXml),
        ContextMenuIcon::Collapse => Some(SvgIcon::Shrink),
        ContextMenuIcon::Copy => Some(SvgIcon::Copy),
        ContextMenuIcon::Cut => Some(SvgIcon::Scissors),
        ContextMenuIcon::Delete => Some(SvgIcon::Trash2),
        ContextMenuIcon::Edit => Some(SvgIcon::Pencil),
        ContextMenuIcon::Expand => Some(SvgIcon::Expand),
        ContextMenuIcon::File => Some(SvgIcon::File),
        ContextMenuIcon::Folder => Some(SvgIcon::Folder),
        ContextMenuIcon::FolderAdd => Some(SvgIcon::FolderPlus),
        ContextMenuIcon::FolderRemove => Some(SvgIcon::FolderMinus),
        ContextMenuIcon::Home => Some(SvgIcon::House),
        ContextMenuIcon::Info => Some(SvgIcon::Info),
        ContextMenuIcon::MoveRight => Some(SvgIcon::ArrowRight),
        ContextMenuIcon::New => Some(SvgIcon::Plus),
        ContextMenuIcon::Note => Some(SvgIcon::NotebookTabs),
        ContextMenuIcon::Notification => Some(SvgIcon::Bell),
        ContextMenuIcon::Paste => Some(SvgIcon::ClipboardPaste),
        ContextMenuIcon::Pin => Some(SvgIcon::Pin),
        ContextMenuIcon::Refresh => Some(SvgIcon::RotateCcw),
        ContextMenuIcon::Redo => Some(SvgIcon::RotateCw),
        ContextMenuIcon::Save => Some(SvgIcon::Save),
        ContextMenuIcon::Search => Some(SvgIcon::Search),
        ContextMenuIcon::Server => Some(SvgIcon::Server),
        ContextMenuIcon::Settings => Some(SvgIcon::Settings),
        ContextMenuIcon::Sidebar => Some(SvgIcon::PanelLeft),
        ContextMenuIcon::Spellcheck => Some(SvgIcon::SpellCheck),
        ContextMenuIcon::SplitHorizontal => Some(SvgIcon::SplitHorizontal),
        ContextMenuIcon::SplitVertical => Some(SvgIcon::SplitVertical),
        ContextMenuIcon::Stack | ContextMenuIcon::Window => Some(SvgIcon::SquareStack),
        ContextMenuIcon::Terminal => Some(SvgIcon::Terminal),
        ContextMenuIcon::Undo => Some(SvgIcon::RotateCcw),
        ContextMenuIcon::Unpin => Some(SvgIcon::PinOff),
        ContextMenuIcon::Vault => Some(SvgIcon::FolderTree),
        ContextMenuIcon::Warning => Some(SvgIcon::CircleAlert),
    }
}
