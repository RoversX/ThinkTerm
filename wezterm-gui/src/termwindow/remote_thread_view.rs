use crate::quad::TripleLayerQuadAllocator;
use crate::ssh_hosts;
use crate::termwindow::content_view::{ContentView, ContentViewResponse, RemoteConnectPhase};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::TermWindow;
use crate::ui::{
    rect, ControlState, DrawContext, InteractionState, UiContext, UiPalette, WidgetKind,
};
use crate::workspace_threads::{self, ThreadConnectionState};
use mux::Mux;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wezterm_font::LoadedFont;
use wezterm_term::{KeyCode, KeyModifiers};
use window::{MouseEventKind as WMEK, MousePress, RectF, WindowOps};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const CONTENT_MAX_W: f32 = 860.0;
const PAD: f32 = 48.0;
const CARD_RADIUS: f32 = 18.0;
const BUTTON_RADIUS: f32 = 14.0;
const BUTTON_MIN_H: f32 = 52.0;
const HERO_ICON_SIZE: f32 = 64.0;
pub(crate) const REMOTE_THREAD_CONTENT_VIEW_KEY_PREFIX: &str = "remote-thread:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteThreadAction {
    Connect,
    EndThread,
    Cancel,
    Retry,
}

/// Lifecycle of the view's own UI: disconnected (offer Connect), in-flight
/// connection (spinner + Cancel), or a failed attempt (error + Retry/Cancel).
enum ViewPhase {
    Idle,
    Connecting { started: Instant },
    Failed { message: String },
}

pub(crate) struct RemoteThreadView {
    state: ThreadConnectionState,
    endpoint: String,
    stale_message: Option<String>,
    phase: ViewPhase,
    widgets: UiContext<RemoteThreadAction>,
    interaction: InteractionState<RemoteThreadAction>,
}

struct RemoteThreadSnapshot {
    state: ThreadConnectionState,
    endpoint: String,
    stale_message: Option<String>,
}

impl RemoteThreadView {
    pub(crate) fn new(state: ThreadConnectionState) -> Self {
        let snapshot = remote_thread_snapshot(state);

        Self {
            state: snapshot.state,
            endpoint: snapshot.endpoint,
            stale_message: snapshot.stale_message,
            phase: ViewPhase::Idle,
            widgets: UiContext::default(),
            interaction: InteractionState::default(),
        }
    }

    fn display_title(&self) -> String {
        if self.state.project_name.trim().is_empty() {
            self.state.thread_name.clone()
        } else {
            format!("{} · {}", self.state.thread_name, self.state.project_name)
        }
    }

    fn apply_snapshot(&mut self, snapshot: RemoteThreadSnapshot) {
        self.state = snapshot.state;
        self.endpoint = snapshot.endpoint;
        self.stale_message = snapshot.stale_message;
    }

    fn paint_impl(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        self.widgets.clear();
        ctx.draw_rect(
            layers,
            0,
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            palette.window_bg,
        )?;

        let shell_w = (area.size.width - PAD * 2.0).min(CONTENT_MAX_W).max(0.0);
        let x = area.origin.x + ((area.size.width - shell_w) / 2.0).max(PAD);
        let mut y = area.origin.y + (area.size.height * 0.12).clamp(PAD, 112.0);
        let width = shell_w;
        let line_h = (ctx.metrics.cell_size.height as f32 + 8.0).max(34.0);

        let is_error =
            matches!(self.phase, ViewPhase::Failed { .. }) || self.stale_message.is_some();
        let hero_icon = if is_error {
            SvgIcon::CircleAlert
        } else {
            SvgIcon::Server
        };
        ctx.draw_rounded_frame(
            layers,
            0,
            x,
            y + 4.0,
            HERO_ICON_SIZE,
            HERO_ICON_SIZE,
            palette.control_bg,
            palette.control_border,
            20.0,
        )?;
        ctx.draw_svg_icon(
            layers,
            hero_icon,
            x + 18.0,
            y + 22.0,
            28.0,
            if is_error {
                palette.secondary_text
            } else {
                palette.text
            },
        )?;
        let hero_text_x = x + HERO_ICON_SIZE + 22.0;
        let hero_text_w = (width - HERO_ICON_SIZE - 22.0).max(0.0);
        ctx.draw_text(
            layers,
            title_font,
            hero_text_x,
            y,
            &self.display_title(),
            palette.text,
            hero_text_w,
        )?;

        let status = match &self.phase {
            ViewPhase::Connecting { .. } => format!("Connecting to {}…", self.endpoint),
            ViewPhase::Failed { message } => message.clone(),
            ViewPhase::Idle => self
                .stale_message
                .clone()
                .unwrap_or_else(|| "This SSH thread is not connected.".to_string()),
        };
        ctx.draw_text(
            layers,
            font,
            hero_text_x,
            y + line_h + 16.0,
            &status,
            palette.muted_text,
            hero_text_w,
        )?;
        y += 112.0;

        let info_h = 214.0;
        self.paint_info_panel(ctx, layers, font, palette, x, y, width, info_h, line_h)?;
        y += info_h + 22.0;

        let note_h = line_h * 2.0 + 34.0;
        self.paint_status_panel(ctx, layers, font, palette, x, y, width, note_h, line_h)?;
        y += note_h + 28.0;

        let button_h = (line_h + 22.0).max(BUTTON_MIN_H);
        self.paint_phase_buttons(ctx, layers, font, palette, x, y, button_h)?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_info_panel(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        line_h: f32,
    ) -> anyhow::Result<()> {
        ctx.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            height,
            palette.control_bg,
            palette.control_border,
            CARD_RADIUS,
        )?;
        let pad = 24.0;
        ctx.draw_text(
            layers,
            font,
            x + pad,
            y + 22.0,
            "Connection",
            palette.text,
            width - pad * 2.0,
        )?;
        let row_y = y + 70.0;
        let label_w = (width * 0.32).clamp(118.0, 168.0);
        let details = [
            ("Project", self.state.project_name.as_str()),
            ("Endpoint", self.endpoint.as_str()),
            ("Thread", self.state.thread_name.as_str()),
        ];
        for (idx, (label, value)) in details.iter().enumerate() {
            let y = row_y + idx as f32 * (line_h + 6.0);
            ctx.draw_text(layers, font, x + pad, y, label, palette.muted_text, label_w)?;
            ctx.draw_text(
                layers,
                font,
                x + pad + label_w,
                y,
                value,
                palette.text,
                width - pad * 2.0 - label_w,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_status_panel(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        line_h: f32,
    ) -> anyhow::Result<()> {
        ctx.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            height,
            palette.control_bg,
            palette.control_border,
            CARD_RADIUS,
        )?;
        let pad = 20.0;
        let text_w = width - pad * 2.0;
        let (line_one, line_two) = match &self.phase {
            ViewPhase::Connecting { started } => {
                let elapsed = started.elapsed();
                let frame =
                    SPINNER_FRAMES[(elapsed.as_millis() / 120) as usize % SPINNER_FRAMES.len()];
                (
                    format!("{frame}  Connecting…"),
                    format!("Elapsed {}s", elapsed.as_secs()),
                )
            }
            ViewPhase::Failed { .. } => (
                "The connection failed.".to_string(),
                "Retry, or cancel to dismiss.".to_string(),
            ),
            ViewPhase::Idle => (
                "Connect opens SSH.".to_string(),
                "Delete removes this thread.".to_string(),
            ),
        };
        ctx.draw_text(
            layers,
            font,
            x + pad,
            y + 16.0,
            &line_one,
            palette.text,
            text_w,
        )?;
        ctx.draw_text(
            layers,
            font,
            x + pad,
            y + 16.0 + line_h,
            &line_two,
            palette.muted_text,
            text_w,
        )?;
        Ok(())
    }

    fn paint_phase_buttons(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        x: f32,
        y: f32,
        button_h: f32,
    ) -> anyhow::Result<()> {
        // (label, icon, action, primary, enabled)
        let buttons: Vec<(&str, SvgIcon, RemoteThreadAction, bool, bool)> = match &self.phase {
            ViewPhase::Idle => vec![
                (
                    "Connect",
                    SvgIcon::Link2,
                    RemoteThreadAction::Connect,
                    true,
                    self.can_connect(),
                ),
                (
                    "Delete Thread",
                    SvgIcon::Trash2,
                    RemoteThreadAction::EndThread,
                    false,
                    true,
                ),
            ],
            ViewPhase::Connecting { .. } => {
                vec![(
                    "Cancel",
                    SvgIcon::X,
                    RemoteThreadAction::Cancel,
                    false,
                    true,
                )]
            }
            ViewPhase::Failed { .. } => vec![
                (
                    "Retry",
                    SvgIcon::RotateCcw,
                    RemoteThreadAction::Retry,
                    true,
                    self.can_connect(),
                ),
                (
                    "Cancel",
                    SvgIcon::X,
                    RemoteThreadAction::Cancel,
                    false,
                    true,
                ),
            ],
        };
        let mut bx = x;
        for (label, icon, action, primary, enabled) in buttons {
            let w = (ctx.measure_text_width(font, label) + 92.0).max(168.0);
            self.paint_action_button(
                ctx,
                layers,
                font,
                palette,
                rect(bx, y, w, button_h),
                label,
                icon,
                action,
                primary,
                enabled,
            )?;
            bx += w + 16.0;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_action_button(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        area: RectF,
        label: &str,
        icon: SvgIcon,
        action: RemoteThreadAction,
        primary: bool,
        enabled: bool,
    ) -> anyhow::Result<()> {
        self.widgets.push(area, WidgetKind::Button, action);
        let state = self.button_state(action, primary, enabled);
        let (mut bg, mut border) = state.colors(palette);
        let mut text = palette.text;
        if primary && enabled {
            bg = if state == ControlState::Pressed {
                palette.selected_bg.mul_alpha(0.82)
            } else {
                palette.selected_bg
            };
            border = palette.selected_bg;
            text = palette.selected_text;
        } else if !enabled {
            bg = bg.mul_alpha(0.52);
            text = palette.muted_text;
        }
        ctx.draw_rounded_frame(
            layers,
            0,
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            bg,
            border,
            BUTTON_RADIUS,
        )?;
        let icon_size = 22.0;
        let icon_x = area.origin.x + 18.0;
        let icon_y = area.origin.y + (area.size.height - icon_size) / 2.0;
        ctx.draw_svg_icon(layers, icon, icon_x, icon_y, icon_size, text)?;
        let text_x = icon_x + icon_size + 12.0;
        ctx.draw_text(
            layers,
            font,
            text_x,
            Self::centered_text_y(ctx, area),
            label,
            text,
            area.size.width - (text_x - area.origin.x) - 16.0,
        )?;
        Ok(())
    }

    fn on_mouse_impl(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse {
        let hit = self.widgets.hit_test(x, y).map(|target| target.action);
        match kind {
            WMEK::Move => {
                if self.interaction.hovered != hit {
                    self.interaction.hovered = hit;
                    ContentViewResponse::Redraw
                } else {
                    ContentViewResponse::Ignored
                }
            }
            WMEK::Press(MousePress::Left) => {
                self.interaction.pressed = hit;
                ContentViewResponse::Redraw
            }
            WMEK::Release(MousePress::Left) => {
                let pressed = self.interaction.pressed.take();
                if let (Some(hit), Some(pressed)) = (hit, pressed) {
                    if hit == pressed {
                        return self.apply(hit);
                    }
                }
                ContentViewResponse::Redraw
            }
            _ => ContentViewResponse::Ignored,
        }
    }

    fn apply(&mut self, action: RemoteThreadAction) -> ContentViewResponse {
        match action {
            RemoteThreadAction::Connect | RemoteThreadAction::Retry if self.can_connect() => {
                self.phase = ViewPhase::Connecting {
                    started: Instant::now(),
                };
                begin_connect_response(self.state.thread_id.clone())
            }
            RemoteThreadAction::Connect | RemoteThreadAction::Retry => ContentViewResponse::Ignored,
            RemoteThreadAction::Cancel => {
                self.phase = ViewPhase::Idle;
                self.interaction = InteractionState::default();
                cancel_connect_response()
            }
            RemoteThreadAction::EndThread => end_thread_response(self.state.thread_id.clone()),
        }
    }

    fn button_state(
        &self,
        action: RemoteThreadAction,
        primary: bool,
        enabled: bool,
    ) -> ControlState {
        if !enabled {
            ControlState::Disabled
        } else if self.interaction.pressed == Some(action) {
            ControlState::Pressed
        } else if self.interaction.hovered == Some(action) {
            ControlState::Hovered
        } else if primary {
            ControlState::Active
        } else {
            ControlState::Normal
        }
    }

    fn can_connect(&self) -> bool {
        self.stale_message.is_none()
    }

    fn centered_text_y(ctx: &DrawContext, area: RectF) -> f32 {
        let cell_height = ctx.metrics.cell_size.height as f32;
        area.origin.y + ((area.size.height - cell_height) / 2.0).max(0.0)
    }
}

impl ContentView for RemoteThreadView {
    fn title(&self) -> String {
        self.display_title()
    }

    fn show_in_tab_bar(&self) -> bool {
        false
    }

    fn tab_key(&self) -> Option<String> {
        Some(format!(
            "{}{}",
            REMOTE_THREAD_CONTENT_VIEW_KEY_PREFIX, self.state.thread_id
        ))
    }

    fn space_id(&self) -> Option<&str> {
        Some(&self.state.space_id)
    }

    fn on_reactivated(&mut self) -> ContentViewResponse {
        // Don't disturb an in-flight or just-failed attempt when the tab is
        // re-focused; the connection poll on TermWindow owns those transitions.
        if matches!(
            self.phase,
            ViewPhase::Connecting { .. } | ViewPhase::Failed { .. }
        ) {
            return ContentViewResponse::Redraw;
        }

        let live_workspaces = Mux::get().iter_workspaces();
        let Some(state) =
            workspace_threads::thread_connection_state(&self.state.thread_id, &live_workspaces)
        else {
            self.stale_message = Some("This saved SSH thread no longer exists.".to_string());
            return ContentViewResponse::Redraw;
        };

        if state.is_live {
            return reveal_live_thread_response(state.thread_id);
        }

        self.apply_snapshot(remote_thread_snapshot(state));
        ContentViewResponse::Redraw
    }

    fn next_frame_time(&self) -> Option<Instant> {
        match self.phase {
            ViewPhase::Connecting { .. } => Some(Instant::now() + Duration::from_millis(120)),
            _ => None,
        }
    }

    fn on_remote_connect_phase(&mut self, phase: RemoteConnectPhase) {
        match phase {
            RemoteConnectPhase::Connecting => {
                if !matches!(self.phase, ViewPhase::Connecting { .. }) {
                    self.phase = ViewPhase::Connecting {
                        started: Instant::now(),
                    };
                }
            }
            RemoteConnectPhase::Failed { message } => {
                self.interaction = InteractionState::default();
                self.phase = ViewPhase::Failed { message };
            }
        }
    }

    fn paint(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
        _section_font: &Rc<LoadedFont>,
        _cursor_on: bool,
    ) -> anyhow::Result<()> {
        self.paint_impl(ctx, layers, area, palette, font, title_font)
    }

    fn on_mouse(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse {
        self.on_mouse_impl(x, y, kind)
    }

    fn on_key(&mut self, key: KeyCode, _mods: KeyModifiers) -> ContentViewResponse {
        match key {
            KeyCode::Escape => self.on_close_requested(),
            _ => ContentViewResponse::Ignored,
        }
    }

    fn on_close_requested(&mut self) -> ContentViewResponse {
        match self.phase {
            ViewPhase::Connecting { .. } => cancel_and_close_response(),
            ViewPhase::Idle | ViewPhase::Failed { .. } => ContentViewResponse::Close,
        }
    }
}

fn remote_thread_snapshot(state: ThreadConnectionState) -> RemoteThreadSnapshot {
    let host_id = workspace_threads::remote_host_id_for_project_id(&state.project_id);
    let spec = ssh_hosts::host_spec(host_id);
    let endpoint = spec
        .as_ref()
        .map(format_endpoint)
        .unwrap_or_else(|| state.project_name.clone());
    let stale_message = if state.is_remote && spec.is_none() {
        Some("The saved SSH host for this thread no longer exists.".to_string())
    } else {
        None
    };

    RemoteThreadSnapshot {
        state,
        endpoint,
        stale_message,
    }
}

/// Already-live thread: close this view and switch to the running terminal.
fn reveal_live_thread_response(thread_id: String) -> ContentViewResponse {
    ContentViewResponse::Run(Box::new(move |tw: &mut TermWindow| {
        let window = tw.window.as_ref().cloned();
        tw.close_content_view();
        if let Some(window) = window {
            tw.activate_workspace_thread(thread_id, &window);
        }
    }))
}

/// Begin a connection while keeping this view foreground as the "Connecting…"
/// UI. `content_view_response_tab_id` is set to this view's id during dispatch.
fn begin_connect_response(thread_id: String) -> ContentViewResponse {
    ContentViewResponse::Run(Box::new(move |tw: &mut TermWindow| {
        let Some(view_id) = tw.content_view_response_tab_id else {
            return;
        };
        let Some(window) = tw.window.as_ref().cloned() else {
            return;
        };
        tw.begin_remote_thread_connection(thread_id, view_id, &window);
    }))
}

/// Abort an in-flight connection (kills the background SSH window) and return to
/// the disconnected view.
fn cancel_connect_response() -> ContentViewResponse {
    ContentViewResponse::Run(Box::new(move |tw: &mut TermWindow| {
        if let Some(view_id) = tw.content_view_response_tab_id {
            tw.cancel_remote_thread_connection(view_id);
        }
    }))
}

fn cancel_and_close_response() -> ContentViewResponse {
    ContentViewResponse::Run(Box::new(move |tw: &mut TermWindow| {
        if let Some(view_id) = tw.content_view_response_tab_id {
            tw.cancel_remote_thread_connection(view_id);
            tw.close_content_view_by_id(view_id);
        }
    }))
}

fn end_thread_response(thread_id: String) -> ContentViewResponse {
    ContentViewResponse::Run(Box::new(move |tw: &mut TermWindow| {
        tw.close_content_view();
        let window = tw.window.as_ref().cloned();
        tw.end_workspace_thread(&thread_id, window.as_ref().map(|w| w as &dyn WindowOps));
    }))
}

fn format_endpoint(spec: &ssh_hosts::SshHostSpec) -> String {
    let host = match spec.port {
        Some(port) if port != 22 => format!("{}:{port}", spec.host),
        _ => spec.host.clone(),
    };
    match &spec.username {
        Some(user) if !user.is_empty() => format!("{user}@{host}"),
        _ => host,
    }
}
