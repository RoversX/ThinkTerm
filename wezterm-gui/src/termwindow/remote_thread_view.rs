use crate::quad::TripleLayerQuadAllocator;
use crate::ssh_hosts;
use crate::termwindow::content_view::{ContentView, ContentViewResponse, RemoteConnectPhase};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::TermWindow;
use crate::ui::{
    draw_scrollbar, rect, wheel_delta_pixels, ButtonVariant, ControlState, DrawContext,
    InteractionState,
    ScrollState, UiContext, UiPalette, UiTokens, WidgetKind,
};
use crate::workspace_threads::{self, ThreadConnectionState};
use fluent_bundle::FluentArgs;
use mux::Mux;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wezterm_font::LoadedFont;
use wezterm_term::{KeyCode, KeyModifiers};
use window::{MouseEventKind as WMEK, MousePress, RectF, WindowOps};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const CONTENT_MAX_W: f32 = 860.0;
const PAD: f32 = 48.0;
const CARD_RADIUS: f32 = 24.0;
const BUTTON_MIN_H: f32 = 52.0;
const HERO_ICON_SIZE: f32 = 64.0;
pub(crate) const REMOTE_THREAD_CONTENT_VIEW_KEY_PREFIX: &str = "remote-thread:";

fn remote_thread_tr(id: &'static str, values: &[(&'static str, String)]) -> String {
    let mut args = FluentArgs::new();
    for (name, value) in values {
        args.set(*name, value.clone());
    }
    crate::i18n::tr_args(id, &args)
}

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
    uses_mosh: bool,
    phase: ViewPhase,
    widgets: UiContext<RemoteThreadAction>,
    interaction: InteractionState<RemoteThreadAction>,
    scroll: ScrollState,
    /// UI scale captured at paint time; mouse handlers get no `DrawContext`.
    last_ui_scale: f32,
}

struct RemoteThreadSnapshot {
    state: ThreadConnectionState,
    endpoint: String,
    stale_message: Option<String>,
    uses_mosh: bool,
}

impl RemoteThreadView {
    pub(crate) fn new(state: ThreadConnectionState) -> Self {
        let snapshot = remote_thread_snapshot(state);

        Self {
            state: snapshot.state,
            endpoint: snapshot.endpoint,
            stale_message: snapshot.stale_message,
            uses_mosh: snapshot.uses_mosh,
            phase: ViewPhase::Idle,
            widgets: UiContext::default(),
            interaction: InteractionState::default(),
            scroll: ScrollState::new(),
            last_ui_scale: 1.0,
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
        self.uses_mosh = snapshot.uses_mosh;
    }

    fn transport_label(&self) -> &'static str {
        if self.uses_mosh {
            "Mosh"
        } else {
            "SSH"
        }
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
        self.last_ui_scale = ctx.scale();
        ctx.draw_rect(
            layers,
            0,
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            palette.window_bg,
        )?;

        let pad = ctx.px(PAD);
        let shell_w = (area.size.width - pad * 2.0)
            .min(ctx.px(CONTENT_MAX_W))
            .max(0.0);
        let x = area.origin.x + ((area.size.width - shell_w) / 2.0).max(pad);
        let width = shell_w;
        // cell_size already tracks the window DPI; only the padding and the
        // floor are design pixels.
        let line_h = (ctx.metrics.cell_size.height as f32 + ctx.px(8.0)).max(ctx.px(34.0));

        // Lay the stack out first so the scroll extents are known before
        // anything is drawn.
        let top_margin = (area.size.height * 0.12).clamp(pad, ctx.px(112.0));
        let hero_h = ctx.px(112.0);
        let info_h = ctx.px(214.0);
        let info_gap = ctx.px(22.0);
        let note_h = line_h * 2.0 + ctx.px(34.0);
        let note_gap = ctx.px(28.0);
        let button_h = (line_h + ctx.px(22.0)).max(ctx.px(BUTTON_MIN_H));
        let content_h =
            top_margin + hero_h + info_h + info_gap + note_h + note_gap + button_h + pad;
        self.scroll.set_extents(area.size.height, content_h);

        let mut y = area.origin.y + top_margin - self.scroll.offset;

        let is_error =
            matches!(self.phase, ViewPhase::Failed { .. }) || self.stale_message.is_some();
        let hero_icon = if is_error {
            SvgIcon::CircleAlert
        } else {
            SvgIcon::Server
        };
        let hero_icon_size = ctx.px(HERO_ICON_SIZE);
        ctx.draw_rounded_frame(
            layers,
            0,
            x,
            y + ctx.px(4.0),
            hero_icon_size,
            hero_icon_size,
            palette.control_bg,
            palette.control_border,
            ctx.px(20.0),
        )?;
        ctx.draw_svg_icon(
            layers,
            hero_icon,
            x + ctx.px(18.0),
            y + ctx.px(22.0),
            ctx.px(28.0),
            if is_error {
                palette.secondary_text
            } else {
                palette.text
            },
        )?;
        let hero_text_x = x + hero_icon_size + ctx.px(22.0);
        let hero_text_w = (width - hero_icon_size - ctx.px(22.0)).max(0.0);
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
            ViewPhase::Connecting { .. } => remote_thread_tr(
                "remote-thread-connecting-to",
                &[("endpoint", self.endpoint.clone())],
            ),
            ViewPhase::Failed { message } => message.clone(),
            ViewPhase::Idle => self
                .stale_message
                .clone()
                .unwrap_or_else(|| crate::i18n::tr("remote-thread-not-connected")),
        };
        ctx.draw_text(
            layers,
            font,
            hero_text_x,
            y + line_h + ctx.px(16.0),
            &status,
            palette.muted_text,
            hero_text_w,
        )?;
        y += hero_h;

        self.paint_info_panel(ctx, layers, font, palette, x, y, width, info_h, line_h)?;
        y += info_h + info_gap;

        self.paint_status_panel(ctx, layers, font, palette, x, y, width, note_h, line_h)?;
        y += note_h + note_gap;

        self.paint_phase_buttons(ctx, layers, font, palette, x, y, button_h)?;

        // Nothing here can clip, so scrolled-away content would bleed into the
        // window chrome above. Repaint that strip in the chrome colour; the
        // tab bar / window buttons are drawn after content views and land on
        // top of it again.
        if self.scroll.offset > 0.0 && area.origin.y > 0.0 {
            ctx.draw_rect(
                layers,
                2,
                area.origin.x,
                0.0,
                area.size.width,
                area.origin.y,
                palette.sidebar_bg,
            )?;
        }
        if self.scroll.has_overflow() {
            draw_scrollbar(
                ctx,
                layers,
                palette,
                UiTokens::for_dpi(ctx.dimensions.dpi),
                area,
                self.scroll,
            )?;
        }

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
            ctx.px(CARD_RADIUS),
        )?;
        let pad = ctx.px(24.0);
        ctx.draw_text(
            layers,
            font,
            x + pad,
            y + ctx.px(22.0),
            &crate::i18n::tr("remote-thread-connection"),
            palette.text,
            width - pad * 2.0,
        )?;
        let row_y = y + ctx.px(70.0);
        let label_w = (width * 0.32).clamp(ctx.px(118.0), ctx.px(168.0));
        let details = [
            (
                crate::i18n::tr("remote-thread-project"),
                self.state.project_name.as_str(),
            ),
            (
                crate::i18n::tr("remote-thread-endpoint"),
                self.endpoint.as_str(),
            ),
            (
                crate::i18n::tr("remote-thread-thread"),
                self.state.thread_name.as_str(),
            ),
        ];
        for (idx, (label, value)) in details.iter().enumerate() {
            let y = row_y + idx as f32 * (line_h + ctx.px(6.0));
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
            ctx.px(CARD_RADIUS),
        )?;
        let pad = ctx.px(20.0);
        let text_w = width - pad * 2.0;
        let (line_one, line_two) = match &self.phase {
            ViewPhase::Connecting { started } => {
                let elapsed = started.elapsed();
                let frame =
                    SPINNER_FRAMES[(elapsed.as_millis() / 120) as usize % SPINNER_FRAMES.len()];
                (
                    remote_thread_tr("remote-thread-connecting", &[("frame", frame.to_string())]),
                    remote_thread_tr(
                        "remote-thread-transport-elapsed",
                        &[
                            ("transport", self.transport_label().to_string()),
                            ("seconds", elapsed.as_secs().to_string()),
                        ],
                    ),
                )
            }
            ViewPhase::Failed { .. } => (
                crate::i18n::tr("remote-thread-failed"),
                remote_thread_tr(
                    "remote-thread-failed-detail",
                    &[("transport", self.transport_label().to_string())],
                ),
            ),
            ViewPhase::Idle => {
                let detail = if self.uses_mosh {
                    crate::i18n::tr("remote-thread-mosh-detail")
                } else {
                    crate::i18n::tr("remote-thread-ssh-detail")
                };
                (
                    remote_thread_tr(
                        "remote-thread-transport",
                        &[("transport", self.transport_label().to_string())],
                    ),
                    detail,
                )
            }
        };
        ctx.draw_text(
            layers,
            font,
            x + pad,
            y + ctx.px(16.0),
            &line_one,
            palette.text,
            text_w,
        )?;
        ctx.draw_text(
            layers,
            font,
            x + pad,
            y + ctx.px(16.0) + line_h,
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
        let buttons: Vec<(String, SvgIcon, RemoteThreadAction, bool, bool)> = match &self.phase {
            ViewPhase::Idle => vec![
                (
                    crate::i18n::tr("remote-thread-connect"),
                    SvgIcon::Link2,
                    RemoteThreadAction::Connect,
                    true,
                    self.can_connect(),
                ),
                (
                    crate::i18n::tr("remote-thread-delete"),
                    SvgIcon::Trash2,
                    RemoteThreadAction::EndThread,
                    false,
                    true,
                ),
            ],
            ViewPhase::Connecting { .. } => {
                vec![(
                    crate::i18n::tr("remote-thread-cancel"),
                    SvgIcon::X,
                    RemoteThreadAction::Cancel,
                    false,
                    true,
                )]
            }
            ViewPhase::Failed { .. } => vec![
                (
                    crate::i18n::tr("remote-thread-retry"),
                    SvgIcon::RotateCcw,
                    RemoteThreadAction::Retry,
                    true,
                    self.can_connect(),
                ),
                (
                    crate::i18n::tr("remote-thread-cancel"),
                    SvgIcon::X,
                    RemoteThreadAction::Cancel,
                    false,
                    true,
                ),
            ],
        };
        let mut bx = x;
        for (label, icon, action, primary, enabled) in buttons {
            let w = (ctx.measure_text_width(font, &label) + ctx.px(92.0)).max(ctx.px(168.0));
            self.paint_action_button(
                ctx,
                layers,
                font,
                palette,
                rect(bx, y, w, button_h),
                &label,
                icon,
                action,
                primary,
                enabled,
            )?;
            bx += w + ctx.px(16.0);
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
        let state = self.button_state(action, enabled);
        // Shared with every other button in the app, so "primary" means the
        // accent in both appearances, and disabled dims consistently instead
        // of by a local rule.
        let variant = if primary {
            ButtonVariant::Primary
        } else {
            ButtonVariant::Secondary
        };
        let (bg, _border, text) = variant.colors(state, palette);
        ctx.draw_rounded_frame(
            layers,
            0,
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            bg,
            // No outline. The fill already separates these from the page, and
            // the stroked ring is drawn as its own curve rather than following
            // the fill: at pill radius its corner arc runs almost horizontally
            // where it meets the top edge, so the antialiasing smears sideways
            // and the end reads as a rounded rectangle. Passing the fill as the
            // border makes draw_rounded_frame skip the ring entirely.
            bg,
            // Fully rounded, like every other button in the app. Derived from
            // the rect instead of a constant so it cannot fall out of step
            // when the button height changes.
            area.size.height / 2.0,
        )?;
        let icon_size = ctx.px(22.0);
        let icon_x = area.origin.x + ctx.px(18.0);
        let icon_y = area.origin.y + (area.size.height - icon_size) / 2.0;
        ctx.draw_svg_icon(layers, icon, icon_x, icon_y, icon_size, text)?;
        let text_x = icon_x + icon_size + ctx.px(12.0);
        ctx.draw_text(
            layers,
            font,
            text_x,
            Self::centered_text_y(ctx, area),
            label,
            text,
            area.size.width - (text_x - area.origin.x) - ctx.px(16.0),
        )?;
        Ok(())
    }

    fn on_mouse_impl(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse {
        let hit = self.widgets.hit_test(x, y).map(|target| target.action);
        match kind {
            WMEK::VertWheel(amount) => {
                let old = self.scroll.offset;
                self.scroll
                    .scroll_by(wheel_delta_pixels(amount, self.last_ui_scale));
                if (self.scroll.offset - old).abs() > 0.01 {
                    ContentViewResponse::Redraw
                } else {
                    ContentViewResponse::Ignored
                }
            }
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

    fn button_state(&self, action: RemoteThreadAction, enabled: bool) -> ControlState {
        if !enabled {
            ControlState::Disabled
        } else if self.interaction.pressed == Some(action) {
            ControlState::Pressed
        } else if self.interaction.hovered == Some(action) {
            ControlState::Hovered
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
            self.stale_message = Some(crate::i18n::tr("remote-thread-stale"));
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
        Some(crate::i18n::tr("remote-thread-host-stale"))
    } else {
        None
    };
    let uses_mosh = spec.as_ref().is_some_and(|spec| spec.use_mosh);

    RemoteThreadSnapshot {
        state,
        endpoint,
        stale_message,
        uses_mosh,
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
