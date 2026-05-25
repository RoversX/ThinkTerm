use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER,
};
use crate::termwindow::render::RenderScreenLineParams;
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::ui::tokens::{
    mix, SIDEBAR_ICON_GAP, SIDEBAR_INSET, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH,
    SIDEBAR_RESIZE_HANDLE_WIDTH, SIDEBAR_ROW_RADIUS, SIDEBAR_WIDTH_CELLS,
};
use crate::termwindow::{UIItem, UIItemType};
use crate::utilsprites::RenderMetrics;
use anyhow::Context;
use mux::renderable::RenderableDimensions;
use mux::Mux;
use std::rc::Rc;
use wezterm_font::LoadedFont;
use wezterm_term::color::{ColorAttribute, ColorPalette};
use wezterm_term::{CellAttributes, Line};
use window::color::LinearRgba;
use window::RectF;

#[derive(Debug, Clone, Copy)]
pub struct WorkspaceSidebarRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

pub fn workspace_sidebar_width_for_metrics(render_metrics: &RenderMetrics) -> usize {
    (render_metrics.cell_size.width as usize * SIDEBAR_WIDTH_CELLS).max(SIDEBAR_MIN_WIDTH)
}

impl crate::TermWindow {
    pub fn workspace_sidebar_width(&self) -> usize {
        if self.workspace_sidebar_collapsed {
            0
        } else {
            self.workspace_sidebar_width
                .clamp(SIDEBAR_MIN_WIDTH, self.workspace_sidebar_max_width())
        }
    }

    pub fn workspace_sidebar_max_width(&self) -> usize {
        SIDEBAR_MAX_WIDTH.min((self.dimensions.pixel_width / 2).max(SIDEBAR_MIN_WIDTH))
    }

    pub fn set_workspace_sidebar_width(&mut self, width: usize) {
        self.workspace_sidebar_width =
            width.clamp(SIDEBAR_MIN_WIDTH, self.workspace_sidebar_max_width());
    }

    pub fn toggle_workspace_sidebar(&mut self) {
        self.workspace_sidebar_collapsed = !self.workspace_sidebar_collapsed;
    }

    pub fn expand_workspace_sidebar(&mut self) {
        self.workspace_sidebar_collapsed = false;
    }

    pub fn tab_bar_left_edge(&self) -> usize {
        let border = self.get_os_border();
        border.left.get() as usize + self.workspace_sidebar_width()
    }

    pub fn workspace_sidebar_rect(&self) -> Option<WorkspaceSidebarRect> {
        let border = self.get_os_border();
        let bottom_tab_bar_height = if self.config.tab_bar_at_bottom && self.show_tab_bar {
            self.tab_bar_pixel_height().unwrap_or(0.0).ceil() as usize
        } else {
            0
        };

        let x = border.left.get() as usize;
        let y = border.top.get() as usize;
        let width = self.workspace_sidebar_width().min(
            self.dimensions
                .pixel_width
                .saturating_sub((border.left + border.right).get() as usize),
        );
        let height = self
            .dimensions
            .pixel_height
            .saturating_sub(y + border.bottom.get() as usize + bottom_tab_bar_height);

        if width == 0 || height == 0 {
            return None;
        }

        Some(WorkspaceSidebarRect {
            x,
            y,
            width,
            height,
        })
    }

    pub fn paint_workspace_sidebar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let rect = match self.workspace_sidebar_rect() {
            Some(rect) => rect,
            None => return Ok(()),
        };

        let palette = self.palette().clone();
        let background = palette
            .resolve_bg(ColorAttribute::Default)
            .to_linear()
            .mul_alpha(self.config.window_background_opacity);
        let black = LinearRgba::with_components(0.0, 0.0, 0.0, background.3);
        let foreground = palette.foreground.to_linear();
        let sidebar_bg = mix(background, black, 0.12);
        let divider = foreground.mul_alpha(0.14);
        let selected_bg = palette.selection_bg.to_linear().mul_alpha(0.55);
        let active_fg = palette
            .selection_fg
            .to_linear()
            .when_fully_transparent(foreground);
        let muted_fg = foreground.mul_alpha(0.62);
        let ui_font = self.fonts.title_font().context("sidebar ui font")?;
        let ui_metrics = RenderMetrics::with_font_metrics(&ui_font.metrics());
        let ui_cell_height = ui_metrics.cell_size.height as usize;
        let icon_size = (ui_cell_height + 12).clamp(30, 36);

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
        .context("sidebar background")?;
        self.filled_rectangle(
            layers,
            1,
            euclid::rect(
                (rect.x + rect.width.saturating_sub(1)) as f32,
                rect.y as f32,
                1.0,
                rect.height as f32,
            ),
            divider,
        )
        .context("sidebar divider")?;

        self.ui_items.push(UIItem {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            item_type: UIItemType::WorkspaceSidebarBackground,
        });

        let item_height = (ui_cell_height.max(icon_size) + SIDEBAR_INSET * 2).max(56);
        let item_x = rect.x + SIDEBAR_INSET;
        let item_width = rect.width.saturating_sub(SIDEBAR_INSET * 2 + 1);
        let row_icon_x = item_x + SIDEBAR_INSET;
        let row_text_x = row_icon_x + icon_size + SIDEBAR_ICON_GAP;
        let row_text_width = item_x
            .saturating_add(item_width)
            .saturating_sub(row_text_x + SIDEBAR_INSET);
        self.ui_items.push(UIItem {
            x: rect
                .x
                .saturating_add(rect.width)
                .saturating_sub(SIDEBAR_RESIZE_HANDLE_WIDTH / 2),
            y: rect.y,
            width: SIDEBAR_RESIZE_HANDLE_WIDTH,
            height: rect.height,
            item_type: UIItemType::WorkspaceSidebarResize,
        });

        if self.workspace_sidebar_collapsed {
            return Ok(());
        }

        let mux = Mux::get();
        let active_workspace = mux.active_workspace();
        let mut workspaces = mux.iter_workspaces();
        if !workspaces.iter().any(|name| name == &active_workspace) {
            workspaces.insert(0, active_workspace.clone());
        }

        let mut y = rect.y + SIDEBAR_INSET;
        for workspace in workspaces {
            if y + item_height > rect.y + rect.height {
                break;
            }

            let is_active = workspace == active_workspace;
            if is_active {
                self.fill_rounded_rectangle(
                    layers,
                    0,
                    euclid::rect(
                        item_x as f32,
                        y as f32,
                        item_width as f32,
                        item_height as f32,
                    ),
                    selected_bg,
                    SIDEBAR_ROW_RADIUS,
                )
                .context("sidebar selected workspace")?;
            }

            self.ui_items.push(UIItem {
                x: item_x,
                y,
                width: item_width,
                height: item_height,
                item_type: UIItemType::WorkspaceSidebar(workspace.clone()),
            });

            let icon_y = y + ((item_height.saturating_sub(icon_size)) / 2);
            let text_y = y + ((item_height.saturating_sub(ui_cell_height)) / 2);
            self.paint_sidebar_icon(
                layers,
                SvgIcon::FolderOpen,
                row_icon_x,
                icon_y,
                icon_size,
                if is_active { active_fg } else { muted_fg },
            )?;
            self.paint_sidebar_text(
                layers,
                &palette,
                &ui_font,
                ui_metrics,
                &workspace,
                row_text_x,
                text_y,
                row_text_width,
                if is_active { active_fg } else { foreground },
            )?;

            y += item_height + (SIDEBAR_INSET / 2);
        }

        Ok(())
    }

    pub(crate) fn fill_rounded_rectangle(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer_num: usize,
        rect: RectF,
        color: LinearRgba,
        radius: f32,
    ) -> anyhow::Result<()> {
        let radius = radius.min(rect.width() / 2.0).min(rect.height() / 2.0);
        if radius <= 0.0 {
            self.filled_rectangle(layers, layer_num, rect, color)?;
            return Ok(());
        }

        let corner_size = euclid::size2(radius, radius);
        self.poly_quad(
            layers,
            layer_num,
            euclid::point2(rect.min_x(), rect.min_y()),
            TOP_LEFT_ROUNDED_CORNER,
            0,
            corner_size,
            color,
        )?
        .set_grayscale();
        self.poly_quad(
            layers,
            layer_num,
            euclid::point2(rect.max_x() - radius, rect.min_y()),
            TOP_RIGHT_ROUNDED_CORNER,
            0,
            corner_size,
            color,
        )?
        .set_grayscale();
        self.poly_quad(
            layers,
            layer_num,
            euclid::point2(rect.min_x(), rect.max_y() - radius),
            BOTTOM_LEFT_ROUNDED_CORNER,
            0,
            corner_size,
            color,
        )?
        .set_grayscale();
        self.poly_quad(
            layers,
            layer_num,
            euclid::point2(rect.max_x() - radius, rect.max_y() - radius),
            BOTTOM_RIGHT_ROUNDED_CORNER,
            0,
            corner_size,
            color,
        )?
        .set_grayscale();

        self.filled_rectangle(
            layers,
            layer_num,
            euclid::rect(
                rect.min_x() + radius,
                rect.min_y(),
                rect.width() - radius * 2.0,
                rect.height(),
            ),
            color,
        )?;
        self.filled_rectangle(
            layers,
            layer_num,
            euclid::rect(
                rect.min_x(),
                rect.min_y() + radius,
                radius,
                rect.height() - radius * 2.0,
            ),
            color,
        )?;
        self.filled_rectangle(
            layers,
            layer_num,
            euclid::rect(
                rect.max_x() - radius,
                rect.min_y() + radius,
                radius,
                rect.height() - radius * 2.0,
            ),
            color,
        )?;

        Ok(())
    }

    fn paint_sidebar_text(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        palette: &ColorPalette,
        font: &Rc<LoadedFont>,
        metrics: RenderMetrics,
        text: &str,
        x: usize,
        y: usize,
        width: usize,
        foreground: LinearRgba,
    ) -> anyhow::Result<()> {
        if width == 0 {
            return Ok(());
        }

        let gl_state = self.render_state.as_ref().unwrap();
        let white_space = gl_state.util_sprites.white_space.texture_coords();
        let filled_box = gl_state.util_sprites.filled_box.texture_coords();
        let cell_width = metrics.cell_size.width as usize;
        let cell_height = metrics.cell_size.height as usize;
        let attrs = CellAttributes::blank();
        let line = Line::from_text(text, &attrs, 0, None);

        self.render_screen_line(
            RenderScreenLineParams {
                top_pixel_y: y as f32,
                left_pixel_x: x as f32,
                pixel_width: width as f32,
                stable_line_idx: None,
                line: &line,
                selection: 0..0,
                cursor: &Default::default(),
                palette,
                dims: &RenderableDimensions {
                    cols: (width / cell_width).max(1),
                    physical_top: 0,
                    scrollback_rows: 0,
                    scrollback_top: 0,
                    viewport_rows: 1,
                    dpi: self.terminal_size.dpi,
                    pixel_height: cell_height,
                    pixel_width: width,
                    reverse_video: false,
                },
                config: &self.config,
                cursor_border_color: LinearRgba::default(),
                foreground,
                pane: None,
                is_active: true,
                selection_fg: LinearRgba::default(),
                selection_bg: LinearRgba::default(),
                cursor_fg: LinearRgba::default(),
                cursor_bg: LinearRgba::default(),
                cursor_is_default_color: true,
                white_space,
                filled_box,
                window_is_transparent: true,
                default_bg: LinearRgba::TRANSPARENT,
                style: Some(font.style()),
                font: Some(Rc::clone(font)),
                use_pixel_positioning: true,
                render_metrics: metrics,
                font_config: None,
                font_identity: font.id() as u64,
                shape_key: None,
                password_input: false,
            },
            layers,
        )?;

        Ok(())
    }

    fn paint_sidebar_icon(
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

        let mut quad = layers.allocate(1)?;
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
