use crate::colorease::ColorEaseUniform;
use crate::customglyph::{BlockKey, Poly};
use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::renderstate::{RenderContext, RenderState};
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER,
};
use crate::uniforms::UniformBuilder;
use crate::utilsprites::RenderMetrics;
use anyhow::Context;
use config::{configuration, Dimension, GeometryOrigin};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use wezterm_bidi::Direction;
use wezterm_font::{FontConfiguration, LoadedFont};
use window::bitmaps::atlas::OutOfTextureSpace;
use window::color::LinearRgba;
use window::glium;
use window::glium::texture::SrgbTexture2d;
use window::glium::uniforms::{
    MagnifySamplerFilter, MinifySamplerFilter, Sampler, SamplerWrapFunction,
};
use window::glium::{BlendingFunction, LinearBlendingFactor, Surface};
use window::{
    Appearance, Connection, ConnectionOps, Dimensions, MouseButtons, MouseCursor, MouseEvent,
    MouseEventKind, MousePress, RequestedWindowGeometry, Window, WindowEvent, WindowOps,
};

const DEFAULT_WIDTH: usize = 1180;
const DEFAULT_HEIGHT: usize = 760;
const SIDEBAR_WIDTH: f32 = 286.0;
const CONTROL_HEIGHT: f32 = 42.0;
const CONTROL_RADIUS: f32 = 8.0;
const NAV_ROW_RADIUS: f32 = 7.0;
const NAV_ROW_HEIGHT: f32 = 40.0;
const NAV_ROW_STEP: f32 = 44.0;

thread_local! {
    static SETTINGS_WINDOW: RefCell<Option<Rc<RefCell<SettingsWindow>>>> = RefCell::new(None);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsSection {
    General,
    Appearance,
    Terminal,
    Workspaces,
    Keymap,
    WezTermSync,
    Developer,
}

const SECTIONS: &[SettingsSection] = &[
    SettingsSection::General,
    SettingsSection::Appearance,
    SettingsSection::Terminal,
    SettingsSection::Workspaces,
    SettingsSection::Keymap,
    SettingsSection::WezTermSync,
    SettingsSection::Developer,
];

impl SettingsSection {
    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Terminal => "Terminal",
            Self::Workspaces => "Workspaces",
            Self::Keymap => "Keymap",
            Self::WezTermSync => "WezTerm Sync",
            Self::Developer => "Developer",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsAction {
    Select(SettingsSection),
    OpenWezTermConfig,
    PrepareWezTermSync,
}

#[derive(Debug, Clone)]
struct HitTarget {
    rect: window::RectF,
    action: SettingsAction,
}

struct StyleToken<'a> {
    name: &'a str,
    value: &'a str,
    swatch: Option<LinearRgba>,
}

#[derive(Clone, Copy)]
struct SettingsPalette {
    window_bg: LinearRgba,
    sidebar_bg: LinearRgba,
    separator: LinearRgba,
    search_bg: LinearRgba,
    search_border: LinearRgba,
    nav_hover_bg: LinearRgba,
    nav_pressed_bg: LinearRgba,
    nav_selected_bg: LinearRgba,
    control_bg: LinearRgba,
    control_hover_bg: LinearRgba,
    control_pressed_bg: LinearRgba,
    control_border: LinearRgba,
    title: LinearRgba,
    text: LinearRgba,
    secondary_text: LinearRgba,
    muted_text: LinearRgba,
    selected_text: LinearRgba,
    rule: LinearRgba,
}

pub fn show() {
    let already_open = SETTINGS_WINDOW.with(|slot| {
        if let Some(settings) = slot.borrow().as_ref() {
            if let Some(window) = settings.borrow().window.as_ref() {
                window.show();
                window.focus();
                return true;
            }
        }
        false
    });

    if already_open {
        return;
    }

    promise::spawn::spawn(async {
        if let Err(err) = SettingsWindow::open().await {
            log::error!("failed to open settings window: {err:#}");
        }
    })
    .detach();
}

struct SettingsWindow {
    window: Option<Window>,
    dimensions: Dimensions,
    fonts: Rc<FontConfiguration>,
    ui_font: Rc<LoadedFont>,
    title_font: Rc<LoadedFont>,
    metrics: RenderMetrics,
    render_state: Option<RenderState>,
    appearance: Appearance,
    selected: SettingsSection,
    hit_targets: Vec<HitTarget>,
    hovered_action: Option<SettingsAction>,
    pressed_action: Option<SettingsAction>,
    status: String,
}

impl SettingsWindow {
    async fn open() -> anyhow::Result<()> {
        let config = configuration();
        let dpi = window::default_dpi() as usize;
        let fonts = Rc::new(FontConfiguration::new(Some(config.clone()), dpi)?);
        let title_font = fonts.title_font()?;
        let ui_font = Rc::clone(&title_font);
        let metrics = RenderMetrics::with_font_metrics(&ui_font.metrics());
        let appearance = Connection::get()
            .map(|conn| conn.get_appearance())
            .unwrap_or(Appearance::Dark);

        let settings = Rc::new(RefCell::new(Self {
            window: None,
            dimensions: Dimensions {
                pixel_width: DEFAULT_WIDTH,
                pixel_height: DEFAULT_HEIGHT,
                dpi,
            },
            fonts: Rc::clone(&fonts),
            ui_font,
            title_font,
            metrics,
            render_state: None,
            appearance,
            selected: SettingsSection::Appearance,
            hit_targets: Vec::new(),
            hovered_action: None,
            pressed_action: None,
            status: Self::initial_status(),
        }));

        let event_settings = Rc::clone(&settings);
        let geometry = RequestedWindowGeometry {
            width: Dimension::Pixels(DEFAULT_WIDTH as f32),
            height: Dimension::Pixels(DEFAULT_HEIGHT as f32),
            x: None,
            y: None,
            origin: GeometryOrigin::default(),
        };

        let window = Window::new_window(
            "thinkterm-settings",
            "ThinkTerm Settings",
            geometry,
            Some(&config),
            Rc::clone(&fonts),
            move |event, window| {
                if let Err(err) = event_settings.borrow_mut().dispatch(event, window) {
                    log::error!("settings window event failed: {err:#}");
                }
            },
        )
        .await?;

        window.set_title("ThinkTerm Settings");
        let gl = window.enable_opengl().await?;
        settings
            .borrow_mut()
            .created(RenderContext::Glium(Rc::clone(&gl)))?;
        settings.borrow_mut().window.replace(window.clone());

        SETTINGS_WINDOW.with(|slot| slot.replace(Some(settings)));

        window.show();
        window.invalidate();

        Ok(())
    }

    fn created(&mut self, context: RenderContext) -> anyhow::Result<()> {
        self.render_state
            .replace(RenderState::new(context, &self.fonts, &self.metrics, 256)?);
        Ok(())
    }

    fn dispatch(&mut self, event: WindowEvent, window: &Window) -> anyhow::Result<bool> {
        match event {
            WindowEvent::CloseRequested => {
                window.close();
                Ok(true)
            }
            WindowEvent::Destroyed => {
                SETTINGS_WINDOW.with(|slot| {
                    slot.borrow_mut().take();
                });
                Ok(true)
            }
            WindowEvent::Resized { dimensions, .. } => {
                self.dimensions = dimensions;
                window.invalidate();
                Ok(true)
            }
            WindowEvent::NeedRepaint => Ok(self.do_paint(window)),
            WindowEvent::MouseEvent(event) => {
                self.mouse_event(event, window);
                Ok(true)
            }
            WindowEvent::MouseLeave => {
                self.hovered_action = None;
                self.pressed_action = None;
                window.set_cursor(Some(MouseCursor::Arrow));
                window.invalidate();
                Ok(true)
            }
            WindowEvent::AppearanceChanged(appearance) => {
                self.appearance = appearance;
                window.invalidate();
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    fn mouse_event(&mut self, event: MouseEvent, window: &Window) {
        let x = event.coords.x as f32;
        let y = event.coords.y as f32;
        let action = self.action_at(x, y);

        match event.kind {
            MouseEventKind::Move => {
                if self.hovered_action != action {
                    self.hovered_action = action;
                    window.set_cursor(Some(if action.is_some() {
                        MouseCursor::Hand
                    } else {
                        MouseCursor::Arrow
                    }));
                    window.invalidate();
                }
            }
            MouseEventKind::Press(MousePress::Left) => {
                self.hovered_action = action;
                self.pressed_action = action;
                if action.is_some() {
                    window.invalidate();
                }
            }
            MouseEventKind::Release(MousePress::Left) => {
                let pressed = self.pressed_action.take();
                self.hovered_action = action;
                if pressed.is_some() && pressed == action {
                    self.perform_action(action.unwrap());
                }
                window.invalidate();
            }
            _ if event.mouse_buttons == MouseButtons::NONE => {
                if self.pressed_action.take().is_some() {
                    window.invalidate();
                }
            }
            _ => {}
        }
    }

    fn action_at(&self, x: f32, y: f32) -> Option<SettingsAction> {
        self.hit_targets
            .iter()
            .find(|target| target.rect.contains(euclid::point2(x, y)))
            .map(|target| target.action)
    }

    fn palette(&self) -> SettingsPalette {
        match self.appearance {
            Appearance::Light | Appearance::LightHighContrast => SettingsPalette {
                window_bg: rgb(242, 242, 247),
                sidebar_bg: rgb(246, 246, 248),
                separator: rgba(60, 60, 67, 0.18),
                search_bg: rgba(255, 255, 255, 0.92),
                search_border: rgba(60, 60, 67, 0.20),
                nav_hover_bg: rgba(60, 60, 67, 0.08),
                nav_pressed_bg: rgba(60, 60, 67, 0.14),
                nav_selected_bg: rgba(0, 122, 255, 0.88),
                control_bg: rgba(255, 255, 255, 0.90),
                control_hover_bg: rgba(249, 249, 251, 0.96),
                control_pressed_bg: rgba(232, 242, 255, 0.98),
                control_border: rgba(60, 60, 67, 0.20),
                title: rgb(28, 28, 30),
                text: rgb(28, 28, 30),
                secondary_text: rgb(72, 72, 74),
                muted_text: rgb(142, 142, 147),
                selected_text: rgb(255, 255, 255),
                rule: rgba(60, 60, 67, 0.16),
            },
            Appearance::Dark | Appearance::DarkHighContrast => SettingsPalette {
                window_bg: rgb(28, 28, 30),
                sidebar_bg: rgb(36, 36, 38),
                separator: rgba(84, 84, 88, 0.55),
                search_bg: rgba(58, 58, 60, 0.72),
                search_border: rgba(99, 99, 102, 0.48),
                nav_hover_bg: rgba(99, 99, 102, 0.26),
                nav_pressed_bg: rgba(99, 99, 102, 0.36),
                nav_selected_bg: rgba(10, 132, 255, 0.86),
                control_bg: rgba(58, 58, 60, 0.70),
                control_hover_bg: rgba(72, 72, 74, 0.76),
                control_pressed_bg: rgba(64, 82, 112, 0.88),
                control_border: rgba(99, 99, 102, 0.46),
                title: rgb(242, 242, 247),
                text: rgb(242, 242, 247),
                secondary_text: rgb(199, 199, 204),
                muted_text: rgb(142, 142, 147),
                selected_text: rgb(255, 255, 255),
                rule: rgba(84, 84, 88, 0.46),
            },
        }
    }

    fn perform_action(&mut self, action: SettingsAction) {
        match action {
            SettingsAction::Select(section) => {
                self.selected = section;
            }
            SettingsAction::OpenWezTermConfig => {
                if let Some(path) = Self::wezterm_config_source() {
                    self.status = format!("Opening {}", path.display());
                    Self::open_path(path);
                } else {
                    self.status = "No wezterm.lua was found yet".to_string();
                }
            }
            SettingsAction::PrepareWezTermSync => {
                self.status = match Self::wezterm_config_source() {
                    Some(path) => format!(
                        "Ready to sync from {}. Import backend is intentionally pending.",
                        path.display()
                    ),
                    None => "Create or choose a WezTerm config before syncing".to_string(),
                };
            }
        }
    }

    fn do_paint(&mut self, window: &Window) -> bool {
        let gl = match self.render_state.as_ref().map(|state| &state.context) {
            Some(RenderContext::Glium(gl)) => Rc::clone(gl),
            _ => return false,
        };

        if gl.is_context_lost() {
            log::error!("settings window opengl context was lost");
            window.close();
            return false;
        }

        for _ in 0..3 {
            match self.paint_pass() {
                Ok(()) => match self.render_state.as_mut().unwrap().allocated_more_quads() {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(err) => {
                        log::error!("settings window quad allocation failed: {err:#}");
                        break;
                    }
                },
                Err(err) => {
                    if let Some(&OutOfTextureSpace {
                        size: Some(size),
                        current_size,
                    }) = err.root_cause().downcast_ref::<OutOfTextureSpace>()
                    {
                        let size = size.max(current_size);
                        if let Err(err) = self
                            .render_state
                            .as_mut()
                            .unwrap()
                            .recreate_texture_atlas(&self.fonts, &self.metrics, Some(size))
                        {
                            log::error!("settings window texture atlas resize failed: {err:#}");
                            break;
                        }
                        continue;
                    }
                    log::error!("settings window paint failed: {err:#}");
                    break;
                }
            }
        }

        let mut frame = glium::Frame::new(
            gl,
            (
                self.dimensions.pixel_width as u32,
                self.dimensions.pixel_height as u32,
            ),
        );
        let result = self.call_draw_glium(&mut frame);
        result.and_then(|_| window.finish_frame(frame)).is_ok()
    }

    fn paint_pass(&mut self) -> anyhow::Result<()> {
        if let Some(render_state) = self.render_state.as_ref() {
            for layer in render_state.layers.borrow().iter() {
                layer.clear_quad_allocation();
            }
        }

        self.hit_targets.clear();
        let layer = self
            .render_state
            .as_ref()
            .context("settings render state not initialized")?
            .layer_for_zindex(0)?;
        let mut layers = layer.quad_allocator();

        self.paint_background(&mut layers)?;
        self.paint_sidebar(&mut layers)?;
        self.paint_content(&mut layers)?;
        self.paint_status(&mut layers)?;

        Ok(())
    }

    fn paint_background(&self, layers: &mut TripleLayerQuadAllocator<'_>) -> anyhow::Result<()> {
        let palette = self.palette();
        let width = self.dimensions.pixel_width as f32;
        let height = self.dimensions.pixel_height as f32;
        self.draw_rect(layers, 0, 0.0, 0.0, width, height, palette.window_bg)?;
        self.draw_rect(
            layers,
            0,
            0.0,
            0.0,
            SIDEBAR_WIDTH,
            height,
            palette.sidebar_bg,
        )?;
        self.draw_rect(
            layers,
            0,
            SIDEBAR_WIDTH,
            0.0,
            1.0,
            height,
            palette.separator,
        )?;
        Ok(())
    }

    fn paint_sidebar(&mut self, layers: &mut TripleLayerQuadAllocator<'_>) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let title_font = Rc::clone(&self.title_font);

        self.draw_text(
            layers,
            &title_font,
            28.0,
            32.0,
            "ThinkTerm Settings",
            palette.title,
            SIDEBAR_WIDTH - 56.0,
        )?;

        self.draw_rounded_frame(
            layers,
            0,
            22.0,
            78.0,
            SIDEBAR_WIDTH - 44.0,
            42.0,
            palette.search_bg,
            palette.search_border,
            CONTROL_RADIUS,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            40.0,
            self.control_text_y(78.0, CONTROL_HEIGHT),
            "Search settings...",
            palette.muted_text,
            SIDEBAR_WIDTH - 72.0,
        )?;

        let mut y = 148.0;
        for &section in SECTIONS {
            let action = SettingsAction::Select(section);
            let selected = section == self.selected;
            let hovered = self.hovered_action == Some(action);
            let pressed = self.pressed_action == Some(action);
            let row_y = y - 6.0;
            let row_x = 18.0;
            let row_width = SIDEBAR_WIDTH - 36.0;
            let row_bg = if selected {
                Some(palette.nav_selected_bg)
            } else if pressed {
                Some(palette.nav_pressed_bg)
            } else if hovered {
                Some(palette.nav_hover_bg)
            } else {
                None
            };
            if let Some(row_bg) = row_bg {
                self.draw_rounded_rect(
                    layers,
                    0,
                    row_x,
                    row_y,
                    row_width,
                    NAV_ROW_HEIGHT,
                    row_bg,
                    NAV_ROW_RADIUS,
                )?;
            }

            self.hit_targets.push(HitTarget {
                rect: euclid::rect(row_x, row_y, row_width, NAV_ROW_HEIGHT),
                action,
            });
            self.draw_text(
                layers,
                &ui_font,
                row_x + 16.0,
                self.control_text_y(row_y, NAV_ROW_HEIGHT),
                section.label(),
                if selected {
                    palette.selected_text
                } else {
                    palette.secondary_text
                },
                row_width - 32.0,
            )?;
            y += NAV_ROW_STEP;
        }

        Ok(())
    }

    fn paint_content(&mut self, layers: &mut TripleLayerQuadAllocator<'_>) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let title_font = Rc::clone(&self.title_font);
        let x = SIDEBAR_WIDTH + 46.0;
        let max_width = self.dimensions.pixel_width as f32 - x - 50.0;

        self.draw_text(
            layers,
            &title_font,
            x,
            54.0,
            self.selected.label(),
            palette.title,
            max_width,
        )?;
        let edit_button_width = 260.0_f32.min((max_width * 0.48).max(210.0));
        self.draw_button(
            layers,
            x + max_width - edit_button_width,
            34.0,
            edit_button_width,
            "Open wezterm.lua",
            SettingsAction::OpenWezTermConfig,
        )?;

        match self.selected {
            SettingsSection::Appearance => self.paint_appearance(layers, x, max_width)?,
            SettingsSection::WezTermSync => self.paint_wezterm_sync(layers, x, max_width)?,
            SettingsSection::General => self.paint_placeholder(
                layers,
                &ui_font,
                x,
                "Startup behavior, default shell, launch directory, and update policy.",
                max_width,
            )?,
            SettingsSection::Terminal => self.paint_placeholder(
                layers,
                &ui_font,
                x,
                "Terminal font, cursor, selection, bell, scrollback, and pane defaults.",
                max_width,
            )?,
            SettingsSection::Workspaces => self.paint_placeholder(
                layers,
                &ui_font,
                x,
                "Workspace list, default workspace, sidebar behavior, and saved layouts.",
                max_width,
            )?,
            SettingsSection::Keymap => self.paint_placeholder(
                layers,
                &ui_font,
                x,
                "Keyboard shortcuts and command palette actions.",
                max_width,
            )?,
            SettingsSection::Developer => self.paint_developer(layers, x, max_width)?,
        }

        Ok(())
    }

    fn paint_appearance(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);

        self.draw_text(
            layers,
            &ui_font,
            x,
            112.0,
            "Theme",
            palette.muted_text,
            max_width,
        )?;
        self.draw_rect(layers, 0, x, 142.0, max_width, 1.0, palette.rule)?;

        let mut y = 186.0;
        self.paint_setting_row(
            layers,
            x,
            y,
            max_width,
            "Theme Mode",
            "Follow macOS appearance and choose light/dark schemes automatically.",
            "System",
        )?;
        y += 102.0;
        self.paint_setting_row(
            layers,
            x,
            y,
            max_width,
            "Light Theme",
            "Theme used when the system is in light appearance.",
            "Apple System Colors Light",
        )?;
        y += 102.0;
        self.paint_setting_row(
            layers,
            x,
            y,
            max_width,
            "Dark Theme",
            "Theme used when the system is in dark appearance.",
            "Apple System Colors",
        )?;
        y += 102.0;
        self.paint_setting_row(
            layers,
            x,
            y,
            max_width,
            "Interface Font",
            "Settings and sidebar use the macOS system UI font, separate from terminal text.",
            "System UI",
        )?;

        Ok(())
    }

    fn paint_developer(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);

        self.draw_text(
            layers,
            &ui_font,
            x,
            112.0,
            "Live settings UI components. The left side is rendered with the same primitives as the real window.",
            palette.secondary_text,
            max_width,
        )?;
        self.draw_rect(layers, 0, x, 146.0, max_width, 1.0, palette.rule)?;

        let preview_width = (max_width * 0.58).min(560.0);
        let notes_x = x + preview_width + 34.0;
        let notes_width = (max_width - preview_width - 34.0).max(240.0);

        self.draw_text(
            layers,
            &ui_font,
            x,
            184.0,
            "Component Preview",
            palette.title,
            preview_width,
        )?;
        self.draw_rect(layers, 0, x, 214.0, preview_width, 1.0, palette.rule)?;
        self.paint_preview_search(layers, x, 238.0, preview_width)?;

        self.draw_text(
            layers,
            &ui_font,
            x,
            322.0,
            "Sidebar Rows",
            palette.title,
            preview_width,
        )?;
        self.paint_preview_sidebar_row(layers, x, 356.0, preview_width, "General", false)?;
        self.paint_preview_sidebar_row(layers, x, 402.0, preview_width, "Appearance", true)?;

        self.draw_text(
            layers,
            &ui_font,
            x,
            468.0,
            "Buttons and Controls",
            palette.title,
            preview_width,
        )?;
        self.paint_preview_button(layers, x, 502.0, 176.0, "Normal Button", false)?;
        self.paint_preview_button(layers, x + 194.0, 502.0, 162.0, "Accent Button", true)?;
        self.paint_preview_control(layers, x, 554.0, 220.0, "System")?;

        self.draw_text(
            layers,
            &ui_font,
            x,
            590.0,
            "Setting Row",
            palette.title,
            preview_width,
        )?;
        self.paint_setting_row(
            layers,
            x,
            626.0,
            preview_width,
            "Theme Mode",
            "Follow macOS appearance.",
            "System",
        )?;

        let component_tokens = [
            StyleToken {
                name: "search.field",
                value: "h42 bg+top border",
                swatch: Some(palette.search_bg),
            },
            StyleToken {
                name: "nav.active.row",
                value: "h40 system selection",
                swatch: Some(palette.nav_selected_bg),
            },
            StyleToken {
                name: "button.normal",
                value: "h42 rounded hover",
                swatch: Some(palette.control_bg),
            },
            StyleToken {
                name: "button.accent",
                value: "h42 active bg",
                swatch: Some(palette.control_pressed_bg),
            },
            StyleToken {
                name: "control.select",
                value: "h42 rounded select",
                swatch: Some(palette.control_bg),
            },
            StyleToken {
                name: "setting.row",
                value: "rule + label/value",
                swatch: Some(palette.rule),
            },
        ];

        let layout_tokens = [
            StyleToken {
                name: "window",
                value: "1180 x 760",
                swatch: None,
            },
            StyleToken {
                name: "sidebar",
                value: "286 px",
                swatch: None,
            },
            StyleToken {
                name: "content",
                value: "46 gap / 50 margin",
                swatch: None,
            },
            StyleToken {
                name: "rows",
                value: "nav40/44 setting102",
                swatch: None,
            },
        ];

        let cell_size = format!(
            "{} x {} px",
            self.metrics.cell_size.width, self.metrics.cell_size.height
        );
        let dpi = format!("{} dpi", self.dimensions.dpi);
        let type_tokens = [
            StyleToken {
                name: "ui.font",
                value: "macOS title font",
                swatch: None,
            },
            StyleToken {
                name: "title.font",
                value: "window title font",
                swatch: None,
            },
            StyleToken {
                name: "cell size",
                value: &cell_size,
                swatch: None,
            },
            StyleToken {
                name: "window dpi",
                value: &dpi,
                swatch: None,
            },
        ];

        self.paint_style_group(
            layers,
            notes_x,
            184.0,
            notes_width,
            "Component Styles",
            &component_tokens,
        )?;
        self.paint_style_group(
            layers,
            notes_x,
            410.0,
            notes_width,
            "Typography",
            &type_tokens,
        )?;
        self.paint_style_group(
            layers,
            notes_x,
            552.0,
            notes_width,
            "Layout",
            &layout_tokens,
        )?;

        Ok(())
    }

    fn paint_preview_search(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        self.draw_text(layers, &ui_font, x, y, "Search Field", palette.title, width)?;
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y + 28.0,
            width.min(360.0),
            CONTROL_HEIGHT,
            palette.search_bg,
            palette.search_border,
            CONTROL_RADIUS,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            x + 18.0,
            self.control_text_y(y + 28.0, CONTROL_HEIGHT),
            "Search settings...",
            palette.muted_text,
            width.min(360.0) - 36.0,
        )?;
        Ok(())
    }

    fn paint_preview_sidebar_row(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        selected: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let row_width = width.min(360.0);
        if selected {
            self.draw_rounded_rect(
                layers,
                0,
                x,
                y,
                row_width,
                36.0,
                palette.nav_selected_bg,
                NAV_ROW_RADIUS,
            )?;
        } else {
            self.draw_rounded_rect(
                layers,
                0,
                x,
                y,
                row_width,
                36.0,
                palette.nav_hover_bg,
                NAV_ROW_RADIUS,
            )?;
        }
        self.draw_text(
            layers,
            &ui_font,
            x + 16.0,
            self.control_text_y(y, 36.0),
            label,
            if selected {
                palette.selected_text
            } else {
                palette.secondary_text
            },
            row_width - 32.0,
        )?;
        Ok(())
    }

    fn paint_preview_button(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        accent: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let bg = if accent {
            palette.nav_selected_bg
        } else {
            palette.control_bg
        };
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            CONTROL_HEIGHT,
            bg,
            palette.control_border,
            CONTROL_RADIUS,
        )?;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            x + 14.0,
            self.control_text_y(y, CONTROL_HEIGHT),
            label,
            if accent {
                palette.selected_text
            } else {
                palette.text
            },
            width - 28.0,
        )?;
        Ok(())
    }

    fn paint_preview_control(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        value: &str,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            CONTROL_HEIGHT,
            palette.control_bg,
            palette.control_border,
            CONTROL_RADIUS,
        )?;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            x + 14.0,
            self.control_text_y(y, CONTROL_HEIGHT),
            value,
            palette.text,
            width - 42.0,
        )?;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            x + width - 28.0,
            self.control_text_y(y, CONTROL_HEIGHT),
            "v",
            palette.muted_text,
            14.0,
        )?;
        Ok(())
    }

    fn paint_style_group(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        title: &str,
        tokens: &[StyleToken<'_>],
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            x,
            y,
            title,
            palette.title,
            width,
        )?;
        self.draw_rect(layers, 0, x, y + 28.0, width, 1.0, palette.rule)?;

        let mut row_y = y + 52.0;
        for token in tokens {
            self.paint_style_token(
                layers,
                x,
                row_y,
                width,
                token.name,
                token.value,
                token.swatch,
            )?;
            row_y += 26.0;
        }

        Ok(())
    }

    fn paint_style_token(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        name: &str,
        value: &str,
        swatch: Option<LinearRgba>,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let name_x = if let Some(color) = swatch {
            self.draw_rect(layers, 0, x, y + 3.0, 14.0, 14.0, color)?;
            self.draw_rect(layers, 0, x, y + 3.0, 14.0, 1.0, rgba(255, 255, 255, 0.18))?;
            x + 22.0
        } else {
            x
        };
        let value_x = x + width * 0.48;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            name_x,
            y,
            name,
            palette.secondary_text,
            (value_x - name_x - 12.0).max(80.0),
        )?;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            value_x,
            y,
            value,
            palette.text,
            (x + width - value_x).max(80.0),
        )?;
        Ok(())
    }

    fn paint_wezterm_sync(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let source = Self::wezterm_config_source()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "No wezterm.lua found".to_string());

        self.draw_text(
            layers,
            &ui_font,
            x,
            112.0,
            "Import and mirror existing WezTerm configuration without overwriting it first.",
            palette.secondary_text,
            max_width,
        )?;
        self.paint_setting_row(
            layers,
            x,
            166.0,
            max_width,
            "Detected Source",
            &source,
            "Read only",
        )?;
        self.paint_setting_row(
            layers,
            x,
            268.0,
            max_width,
            "ThinkTerm Target",
            "Not connected yet",
            "Pending",
        )?;
        self.draw_button(
            layers,
            x,
            390.0,
            240.0,
            "Open WezTerm Config",
            SettingsAction::OpenWezTermConfig,
        )?;
        self.draw_button(
            layers,
            x + 256.0,
            390.0,
            176.0,
            "Prepare Sync",
            SettingsAction::PrepareWezTermSync,
        )?;

        Ok(())
    }

    fn paint_placeholder(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        x: f32,
        body: &str,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.draw_text(
            layers,
            font,
            x,
            118.0,
            body,
            palette.secondary_text,
            max_width,
        )?;
        self.draw_rect(layers, 0, x, 166.0, max_width, 1.0, palette.rule)?;
        Ok(())
    }

    fn paint_setting_row(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        description: &str,
        value: &str,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        self.draw_rect(layers, 0, x, y - 24.0, width, 1.0, palette.rule)?;
        self.draw_text(
            layers,
            &ui_font,
            x + 22.0,
            y,
            label,
            palette.text,
            width * 0.58,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            x + 22.0,
            y + 30.0,
            description,
            palette.secondary_text,
            width * 0.62,
        )?;
        let control_width = 220.0_f32.min(width * 0.34);
        let control_x = x + width - control_width - 28.0;
        let control_y = y + 2.0;
        self.draw_rounded_frame(
            layers,
            0,
            control_x,
            control_y,
            control_width,
            CONTROL_HEIGHT,
            palette.control_bg,
            palette.control_border,
            CONTROL_RADIUS,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            control_x + 14.0,
            self.control_text_y(control_y, CONTROL_HEIGHT),
            value,
            palette.text,
            control_width - 26.0,
        )?;
        Ok(())
    }

    fn paint_status(&self, layers: &mut TripleLayerQuadAllocator<'_>) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let y = self.dimensions.pixel_height as f32 - 42.0;
        self.draw_rect(
            layers,
            0,
            22.0,
            y - 18.0,
            SIDEBAR_WIDTH - 44.0,
            1.0,
            palette.separator,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            28.0,
            y,
            "Cmd+,  Settings",
            palette.muted_text,
            SIDEBAR_WIDTH - 56.0,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            SIDEBAR_WIDTH + 46.0,
            y,
            &self.status,
            palette.muted_text,
            self.dimensions.pixel_width as f32 - SIDEBAR_WIDTH - 96.0,
        )?;
        Ok(())
    }

    fn draw_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        action: SettingsAction,
    ) -> anyhow::Result<()> {
        self.hit_targets.push(HitTarget {
            rect: euclid::rect(x, y, width, CONTROL_HEIGHT),
            action,
        });

        let palette = self.palette();
        let is_hovered = self.hovered_action == Some(action);
        let is_pressed = self.pressed_action == Some(action);
        let background = if is_pressed {
            palette.control_pressed_bg
        } else if is_hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if is_pressed {
            palette.nav_selected_bg
        } else if is_hovered {
            palette.separator
        } else {
            palette.control_border
        };
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            CONTROL_HEIGHT,
            background,
            border,
            CONTROL_RADIUS,
        )?;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            x + 14.0,
            self.control_text_y(y, CONTROL_HEIGHT),
            label,
            palette.text,
            width - 28.0,
        )?;
        Ok(())
    }

    fn control_text_y(&self, y: f32, height: f32) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        y + ((height - cell_height) / 2.0).max(0.0)
    }

    fn draw_rounded_frame(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        fill: LinearRgba,
        border: LinearRgba,
        radius: f32,
    ) -> anyhow::Result<()> {
        self.draw_rounded_rect(layers, layer_num, x, y, width, height, border, radius)?;
        self.draw_rounded_rect(
            layers,
            layer_num,
            x + 1.0,
            y + 1.0,
            width - 2.0,
            height - 2.0,
            fill,
            (radius - 1.0).max(0.0),
        )?;
        Ok(())
    }

    fn draw_rounded_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: LinearRgba,
        radius: f32,
    ) -> anyhow::Result<()> {
        if width <= 0.0 || height <= 0.0 {
            return Ok(());
        }

        let radius = radius.min(width / 2.0).min(height / 2.0).round().max(0.0);
        if radius <= 0.0 {
            return self.draw_rect(layers, layer_num, x, y, width, height, color);
        }

        let corner_size = euclid::size2(radius, radius);
        self.draw_corner(
            layers,
            layer_num,
            x,
            y,
            TOP_LEFT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;
        self.draw_corner(
            layers,
            layer_num,
            x + width - radius,
            y,
            TOP_RIGHT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;
        self.draw_corner(
            layers,
            layer_num,
            x,
            y + height - radius,
            BOTTOM_LEFT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;
        self.draw_corner(
            layers,
            layer_num,
            x + width - radius,
            y + height - radius,
            BOTTOM_RIGHT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;

        self.draw_rect(
            layers,
            layer_num,
            x + radius,
            y,
            width - radius * 2.0,
            height,
            color,
        )?;
        self.draw_rect(
            layers,
            layer_num,
            x,
            y + radius,
            radius,
            height - radius * 2.0,
            color,
        )?;
        self.draw_rect(
            layers,
            layer_num,
            x + width - radius,
            y + radius,
            radius,
            height - radius * 2.0,
            color,
        )?;

        Ok(())
    }

    fn draw_corner(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        polys: &'static [Poly],
        size: euclid::Size2D<f32, window::PixelUnit>,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let render_state = self.render_state.as_ref().unwrap();
        let sprite = render_state
            .glyph_cache
            .borrow_mut()
            .cached_block(
                BlockKey::PolyWithCustomMetrics {
                    polys,
                    underline_height: self.metrics.underline_height,
                    cell_size: euclid::size2(size.width as isize, size.height as isize),
                },
                &self.metrics,
            )?
            .texture_coords();

        let mut quad = layers.allocate(layer_num)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + size.width - left_offset,
            y + size.height - top_offset,
        );
        quad.set_texture(sprite);
        quad.set_fg_color(color);
        quad.set_alt_color_and_mix_value(color, 0.0);
        quad.set_hsv(None);
        quad.set_has_color(false);
        quad.set_grayscale();
        Ok(())
    }

    fn draw_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        if width <= 0.0 || height <= 0.0 {
            return Ok(());
        }

        let render_state = self.render_state.as_ref().unwrap();
        let mut quad = layers.allocate(layer_num)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + width - left_offset,
            y + height - top_offset,
        );
        quad.set_texture(render_state.util_sprites.filled_box.texture_coords());
        quad.set_is_background();
        quad.set_fg_color(color);
        quad.set_hsv(None);
        Ok(())
    }

    fn draw_text(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        x: f32,
        y: f32,
        text: &str,
        color: LinearRgba,
        max_width: f32,
    ) -> anyhow::Result<()> {
        if text.is_empty() || max_width <= 0.0 {
            return Ok(());
        }

        let infos = font.blocking_shape(text, None, Direction::LeftToRight, None, None)?;
        let render_state = self.render_state.as_ref().unwrap();
        let mut glyph_cache = render_state.glyph_cache.borrow_mut();
        let style = font.style();
        let mut pos_x = x;
        let baseline = self.metrics.cell_size.height as f32 + self.metrics.descender.get() as f32;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        let right_edge = x + max_width;

        for info in infos {
            let glyph = glyph_cache.cached_glyph(&info, style, false, font, &self.metrics, 1)?;
            if let Some(texture) = glyph.texture.as_ref() {
                let glyph_x = pos_x + (glyph.x_offset + glyph.bearing_x).get() as f32;
                let glyph_y = y - (glyph.y_offset + glyph.bearing_y).get() as f32 + baseline;
                let width = texture.coords.size.width as f32 * glyph.scale as f32;
                let height = texture.coords.size.height as f32 * glyph.scale as f32;

                if glyph_x + width > right_edge {
                    break;
                }

                let mut quad = layers.allocate(1)?;
                quad.set_position(
                    glyph_x - left_offset,
                    glyph_y - top_offset,
                    glyph_x + width - left_offset,
                    glyph_y + height - top_offset,
                );
                quad.set_texture(texture.texture_coords());
                quad.set_has_color(glyph.has_color);
                quad.set_fg_color(color);
                quad.set_hsv(None);
            }
            pos_x += glyph.x_advance.get() as f32;
            if pos_x > right_edge {
                break;
            }
        }

        Ok(())
    }

    fn call_draw_glium(&mut self, frame: &mut glium::Frame) -> anyhow::Result<()> {
        let render_state = self.render_state.as_ref().unwrap();
        let tex = render_state.glyph_cache.borrow().atlas.texture();
        let tex = tex.downcast_ref::<SrgbTexture2d>().unwrap();

        frame.clear_color(0.0, 0.0, 0.0, 1.0);

        let projection = euclid::Transform3D::<f32, f32, f32>::ortho(
            -(self.dimensions.pixel_width as f32) / 2.0,
            self.dimensions.pixel_width as f32 / 2.0,
            self.dimensions.pixel_height as f32 / 2.0,
            -(self.dimensions.pixel_height as f32) / 2.0,
            -1.0,
            1.0,
        )
        .to_arrays_transposed();

        let alpha_blending = glium::DrawParameters {
            blend: glium::Blend {
                color: BlendingFunction::Addition {
                    source: LinearBlendingFactor::SourceAlpha,
                    destination: LinearBlendingFactor::OneMinusSourceAlpha,
                },
                alpha: BlendingFunction::Addition {
                    source: LinearBlendingFactor::One,
                    destination: LinearBlendingFactor::OneMinusSourceAlpha,
                },
                constant_value: (0.0, 0.0, 0.0, 0.0),
            },
            ..Default::default()
        };

        let atlas_nearest_sampler = Sampler::new(&*tex)
            .wrap_function(SamplerWrapFunction::Clamp)
            .magnify_filter(MagnifySamplerFilter::Nearest)
            .minify_filter(MinifySamplerFilter::Nearest);

        let atlas_linear_sampler = Sampler::new(&*tex)
            .wrap_function(SamplerWrapFunction::Clamp)
            .magnify_filter(MagnifySamplerFilter::Linear)
            .minify_filter(MinifySamplerFilter::Linear);

        let foreground_text_hsb = (1.0_f32, 1.0_f32, 1.0_f32);
        let milliseconds = 0_u32;
        let ease = ColorEaseUniform {
            in_function: [0.0, 0.0, 1.0, 1.0],
            out_function: [0.0, 0.0, 1.0, 1.0],
            in_duration_ms: 0,
            out_duration_ms: 0,
        };
        let subpixel_aa = false;

        for layer in render_state.layers.borrow().iter() {
            for idx in 0..3 {
                let vb = &layer.vb.borrow()[idx];
                let (vertex_count, index_count) = vb.vertex_index_count();
                if vertex_count > 0 {
                    let vertices = vb.current_vb_mut();
                    let mut uniforms = UniformBuilder::default();

                    uniforms.add("projection", &projection);
                    uniforms.add("atlas_nearest_sampler", &atlas_nearest_sampler);
                    uniforms.add("atlas_linear_sampler", &atlas_linear_sampler);
                    uniforms.add("foreground_text_hsb", &foreground_text_hsb);
                    uniforms.add("subpixel_aa", &subpixel_aa);
                    uniforms.add("milliseconds", &milliseconds);
                    uniforms.add_struct("cursor_blink", &ease);
                    uniforms.add_struct("blink", &ease);
                    uniforms.add_struct("rapid_blink", &ease);

                    frame.draw(
                        vertices.glium().slice(0..vertex_count).unwrap(),
                        vb.indices.glium().slice(0..index_count).unwrap(),
                        render_state.glyph_prog.as_ref().unwrap(),
                        &uniforms,
                        &alpha_blending,
                    )?;
                }

                vb.next_index();
            }
        }

        Ok(())
    }

    fn initial_status() -> String {
        match Self::wezterm_config_source() {
            Some(path) => format!("Found WezTerm config at {}", path.display()),
            None => "No existing WezTerm config was found yet".to_string(),
        }
    }

    fn wezterm_config_source() -> Option<PathBuf> {
        let mut candidates = vec![config::HOME_DIR.join(".wezterm.lua")];
        for dir in config::CONFIG_DIRS.iter() {
            candidates.push(dir.join("wezterm.lua"));
        }
        candidates.into_iter().find(|path| path.exists())
    }

    fn open_path(path: PathBuf) {
        match url::Url::from_file_path(&path) {
            Ok(url) => wezterm_open_url::open_url(url.as_str()),
            Err(_) => log::error!("Unable to convert {} into a file URL", path.display()),
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> LinearRgba {
    rgba(r, g, b, 1.0)
}

fn rgba(r: u8, g: u8, b: u8, a: f32) -> LinearRgba {
    LinearRgba::with_components(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a)
}
