//! The content-area page shown when a Thread cannot be opened in its Project
//! directory. It lives in the content area (not the sidebar, which may be
//! closed) and shares its [`FolderProblem`] classification with the Files
//! panel so the two never disagree. Structurally a sibling of
//! `remote_thread_view`.

use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::content_view::{ContentView, ContentViewResponse, ProjectRootProbe};
use crate::termwindow::ui::folder_problem::{FolderProblem, ProjectRootUnavailable};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::ui::right_sidebar::{wrap_path_for_width, wrap_snippet_text_for_width};
use crate::termwindow::{ContentViewId, TermWindow, TermWindowNotif};
use crate::ui::{
    draw_scrollbar, rect, wheel_delta_pixels, ButtonVariant, ControlState, DrawContext,
    InteractionState,
    ScrollState, UiContext, UiPalette, UiTokens, WidgetKind,
};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;
use wezterm_font::LoadedFont;
use window::WindowOps;
use window::{MouseEventKind as WMEK, MousePress, RectF};

const CONTENT_MAX_W: f32 = 860.0;
const PAD: f32 = 48.0;
const CARD_RADIUS: f32 = 18.0;
const BUTTON_RADIUS: f32 = 14.0;
const BUTTON_MIN_H: f32 = 52.0;
const HERO_ICON_SIZE: f32 = 64.0;
const DETAIL_MAX_LINES: usize = 4;
pub(crate) const PROJECT_ROOT_CONTENT_VIEW_KEY_PREFIX: &str = "project-root:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectRootAction {
    Reauthorize,
    OpenThread,
    /// The session server, not the GUI, was refused: a picker cannot grant
    /// for it, so send the user to the switch macOS keeps for it.
    OpenSystemSettings,
}

/// Deliberately only two states: everything beyond these existed to drive an
/// automatic re-open, and each extra state had to be raced against the others.
enum ViewPhase {
    Blocked(ProjectRootUnavailable),
    /// The directory reads again; the button delegates to the ordinary
    /// Thread-activation path.
    Granted,
}

pub(crate) struct ProjectRootView {
    thread_id: String,
    space_id: String,
    display_name: String,
    root: PathBuf,
    phase: ViewPhase,
    /// A re-authorization round trip is outstanding. A plain flag: the button
    /// is not offered while set, which also keeps the modeless macOS panel
    /// from being stacked on itself.
    reauth_in_flight: bool,
    /// The user picked some other folder in the panel; say so rather than
    /// silently repointing the Project.
    other_folder_note: Option<String>,
    widgets: UiContext<ProjectRootAction>,
    interaction: InteractionState<ProjectRootAction>,
    scroll: ScrollState,
    /// UI scale captured at paint time; mouse handlers get no `DrawContext`.
    last_ui_scale: f32,
}

impl ProjectRootView {
    pub(crate) fn new(
        thread_id: String,
        space_id: String,
        display_name: String,
        failure: ProjectRootUnavailable,
    ) -> Self {
        Self {
            thread_id,
            space_id,
            display_name,
            root: failure.path.clone(),
            phase: ViewPhase::Blocked(failure),
            reauth_in_flight: false,
            other_folder_note: None,
            widgets: UiContext::default(),
            interaction: InteractionState::default(),
            scroll: ScrollState::new(),
            last_ui_scale: 1.0,
        }
    }

    fn problem(&self) -> Option<FolderProblem> {
        match &self.phase {
            ViewPhase::Blocked(failure) => Some(failure.problem),
            ViewPhase::Granted => None,
        }
    }

    fn headline(&self) -> String {
        match &self.phase {
            ViewPhase::Blocked(failure) => failure.problem.title(),
            ViewPhase::Granted => crate::i18n::tr("project-root-recovered"),
        }
    }

    /// The explanatory paragraph under the headline.
    fn body(&self) -> Option<String> {
        match &self.phase {
            ViewPhase::Blocked(failure) => {
                let mut body = crate::i18n::tr("project-root-no-terminal");
                if let Some(hint) = failure.problem.hint() {
                    body.push(' ');
                    body.push_str(&hint);
                }
                if failure.via_session_server {
                    body.push(' ');
                    body.push_str(&crate::i18n::tr("project-root-server-refused"));
                }
                Some(body)
            }
            ViewPhase::Granted => Some(crate::i18n::tr("project-root-recovered-note")),
        }
    }

    /// The raw OS refusal, so a bug report can name the errno.
    fn detail(&self) -> Option<&str> {
        match &self.phase {
            ViewPhase::Blocked(failure) => Some(failure.detail.as_str()),
            ViewPhase::Granted => None,
        }
    }

    /// (label, icon, action, primary). macOS only: there a picker selection
    /// grants access (TCC never re-prompts on its own); on other platforms a
    /// picker grants nothing, so the hint text points at the real remedy.
    fn buttons(&self) -> Vec<(String, SvgIcon, ProjectRootAction, bool)> {
        if self.reauth_in_flight || !cfg!(target_os = "macos") {
            return Vec::new();
        }
        match &self.phase {
            ViewPhase::Blocked(failure) if failure.problem.can_reauthorize() => {
                let mut buttons = vec![(
                    crate::i18n::tr("project-root-reauthorize"),
                    SvgIcon::FolderOpen,
                    ProjectRootAction::Reauthorize,
                    true,
                )];
                if failure.via_session_server {
                    buttons.push((
                        crate::i18n::tr("project-root-open-settings"),
                        SvgIcon::Settings,
                        ProjectRootAction::OpenSystemSettings,
                        false,
                    ));
                }
                buttons
            }
            ViewPhase::Blocked(_) => Vec::new(),
            // Delegates to the exact activation a sidebar click triggers;
            // its predecessor's bugs all came from re-implementing that.
            ViewPhase::Granted => vec![(
                crate::i18n::tr("project-root-open-terminal"),
                SvgIcon::SquareTerminal,
                ProjectRootAction::OpenThread,
                true,
            )],
        }
    }

    fn wrap(
        &self,
        ctx: &DrawContext,
        font: &Rc<LoadedFont>,
        text: &str,
        width: f32,
        max_lines: usize,
    ) -> Vec<String> {
        if width <= 0.0 {
            return Vec::new();
        }
        wrap_snippet_text_for_width(text, max_lines, false, |segment| {
            ctx.measure_text_width(font, segment) / width
        })
    }

    #[allow(clippy::too_many_arguments)]
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
        let line_h = (ctx.metrics.cell_size.height as f32 + ctx.px(8.0)).max(ctx.px(34.0));

        // Wrap before laying out: the panel heights depend on how many lines
        // the text actually took, and a hint that wrapped to three lines must
        // not run under the buttons.
        let hero_icon_size = ctx.px(HERO_ICON_SIZE);
        let hero_text_x = x + hero_icon_size + ctx.px(22.0);
        let hero_text_w = (width - hero_icon_size - ctx.px(22.0)).max(0.0);
        let card_pad = ctx.px(20.0);
        let card_text_w = (width - card_pad * 2.0).max(0.0);

        let body_lines = self
            .body()
            .map(|body| self.wrap(ctx, font, &body, card_text_w, 6))
            .unwrap_or_default();
        let path_text = self.root.display().to_string();
        // Path-aware: break after separators, not mid-component.
        let path_lines = if card_text_w <= 0.0 {
            Vec::new()
        } else {
            wrap_path_for_width(&path_text, 2, |segment| {
                ctx.measure_text_width(font, segment) / card_text_w
            })
        };
        let detail_lines = self
            .detail()
            .map(|detail| self.wrap(ctx, font, detail, card_text_w, DETAIL_MAX_LINES))
            .unwrap_or_default();
        let note_lines = self
            .other_folder_note
            .clone()
            .map(|note| self.wrap(ctx, font, &note, card_text_w, 3))
            .unwrap_or_default();
        let buttons = self.buttons();
        let button_widths: Vec<f32> = buttons
            .iter()
            .map(|(label, ..)| {
                (ctx.measure_text_width(font, label) + ctx.px(92.0)).max(ctx.px(168.0))
            })
            .collect();

        let card_lines = path_lines.len() + body_lines.len() + detail_lines.len();
        let card_h = if card_lines == 0 {
            0.0
        } else {
            // One blank line between each group that is present.
            let groups = [
                !path_lines.is_empty(),
                !body_lines.is_empty(),
                !detail_lines.is_empty(),
            ]
            .iter()
            .filter(|present| **present)
            .count();
            line_h * card_lines as f32
                + line_h * groups.saturating_sub(1) as f32 * 0.5
                + card_pad * 2.0
        };
        let note_h = if note_lines.is_empty() {
            0.0
        } else {
            line_h * note_lines.len() as f32 + card_pad * 2.0
        };

        let top_margin = (area.size.height * 0.12).clamp(pad, ctx.px(112.0));
        let hero_h = ctx.px(112.0);
        let card_gap = ctx.px(22.0);
        let button_h = (line_h + ctx.px(22.0)).max(ctx.px(BUTTON_MIN_H));
        let button_gap = ctx.px(16.0);
        // Wrapped to the shell width: three localized labels can outgrow a
        // narrow window, and a button painted outside the content area is a
        // button mouse dispatch will never hit.
        let button_rows = {
            let mut rows = 0usize;
            let mut bx = 0.0f32;
            for w in &button_widths {
                if bx > 0.0 && bx + w > width {
                    bx = 0.0;
                }
                if bx == 0.0 {
                    rows += 1;
                }
                bx += w + button_gap;
            }
            rows
        };
        let button_block_h = if buttons.is_empty() {
            0.0
        } else {
            button_rows as f32 * button_h
                + (button_rows as f32 - 1.0).max(0.0) * button_gap
                + card_gap
        };
        let content_h = top_margin
            + hero_h
            + card_h
            + if card_h > 0.0 { card_gap } else { 0.0 }
            + note_h
            + if note_h > 0.0 { card_gap } else { 0.0 }
            + button_block_h
            + pad;
        self.scroll.set_extents(area.size.height, content_h);

        let mut y = area.origin.y + top_margin - self.scroll.offset;

        let hero_icon = self
            .problem()
            .map(FolderProblem::icon)
            .unwrap_or(SvgIcon::FolderOpen);
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
            palette.secondary_text,
        )?;
        ctx.draw_text(
            layers,
            title_font,
            hero_text_x,
            y,
            &self.headline(),
            palette.text,
            hero_text_w,
        )?;
        ctx.draw_text(
            layers,
            font,
            hero_text_x,
            y + line_h + ctx.px(16.0),
            &self.display_name,
            palette.muted_text,
            hero_text_w,
        )?;
        y += hero_h;

        if card_h > 0.0 {
            ctx.draw_rounded_frame(
                layers,
                0,
                x,
                y,
                width,
                card_h,
                palette.control_bg,
                palette.control_border,
                ctx.px(CARD_RADIUS),
            )?;
            let mut text_y = y + card_pad;
            // The path leads: the user has to recognise WHICH folder before
            // any of the buttons mean anything.
            for line in &path_lines {
                ctx.draw_text(
                    layers,
                    font,
                    x + card_pad,
                    text_y,
                    line,
                    palette.text,
                    card_text_w,
                )?;
                text_y += line_h;
            }
            if !body_lines.is_empty() {
                if !path_lines.is_empty() {
                    text_y += line_h * 0.5;
                }
                for line in &body_lines {
                    ctx.draw_text(
                        layers,
                        font,
                        x + card_pad,
                        text_y,
                        line,
                        palette.muted_text,
                        card_text_w,
                    )?;
                    text_y += line_h;
                }
            }
            if !detail_lines.is_empty() {
                text_y += line_h * 0.5;
                for line in &detail_lines {
                    ctx.draw_text(
                        layers,
                        font,
                        x + card_pad,
                        text_y,
                        line,
                        palette.secondary_text,
                        card_text_w,
                    )?;
                    text_y += line_h;
                }
            }
            y += card_h + card_gap;
        }

        if note_h > 0.0 {
            ctx.draw_rounded_frame(
                layers,
                0,
                x,
                y,
                width,
                note_h,
                palette.control_bg,
                palette.control_border,
                ctx.px(CARD_RADIUS),
            )?;
            let mut text_y = y + card_pad;
            for line in &note_lines {
                ctx.draw_text(
                    layers,
                    font,
                    x + card_pad,
                    text_y,
                    line,
                    palette.muted_text,
                    card_text_w,
                )?;
                text_y += line_h;
            }
            y += note_h + card_gap;
        }

        let mut bx = x;
        for ((label, icon, action, primary), w) in buttons.into_iter().zip(button_widths) {
            if bx > x && bx + w > x + width {
                bx = x;
                y += button_h + button_gap;
            }
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
            )?;
            bx += w + button_gap;
        }

        // Nothing here clips, so scrolled-away content would bleed into the
        // window chrome above.
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
    fn paint_action_button(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        palette: UiPalette,
        area: RectF,
        label: &str,
        icon: SvgIcon,
        action: ProjectRootAction,
        primary: bool,
    ) -> anyhow::Result<()> {
        self.widgets.push(area, WidgetKind::Button, action);
        let state = self.button_state(action);
        // Shared with every other button in the app, so "primary" means the
        // accent in both appearances rather than a grey that only reads as
        // primary in the light one.
        let variant = if primary {
            ButtonVariant::Primary
        } else {
            ButtonVariant::Secondary
        };
        let (bg, border, text) = variant.colors(state, palette);
        ctx.draw_rounded_frame(
            layers,
            0,
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            bg,
            border,
            ctx.px(BUTTON_RADIUS),
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

    fn button_state(&self, action: ProjectRootAction) -> ControlState {
        if self.interaction.pressed == Some(action) {
            ControlState::Pressed
        } else if self.interaction.hovered == Some(action) {
            ControlState::Hovered
        } else {
            ControlState::Normal
        }
    }

    fn centered_text_y(ctx: &DrawContext, area: RectF) -> f32 {
        let cell_height = ctx.metrics.cell_size.height as f32;
        area.origin.y + ((area.size.height - cell_height) / 2.0).max(0.0)
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

    fn apply(&mut self, action: ProjectRootAction) -> ContentViewResponse {
        match action {
            ProjectRootAction::Reauthorize => {
                self.other_folder_note = None;
                self.interaction = InteractionState::default();
                reauthorize_response(self.root.clone())
            }
            ProjectRootAction::OpenThread => open_thread_response(self.thread_id.clone()),
            ProjectRootAction::OpenSystemSettings => {
                open_files_and_folders_settings();
                ContentViewResponse::Redraw
            }
        }
    }
}

impl ContentView for ProjectRootView {
    fn title(&self) -> String {
        self.display_name.clone()
    }

    fn show_in_tab_bar(&self) -> bool {
        false
    }

    fn tab_key(&self) -> Option<String> {
        Some(format!(
            "{}{}",
            PROJECT_ROOT_CONTENT_VIEW_KEY_PREFIX, self.thread_id
        ))
    }

    fn space_id(&self) -> Option<&str> {
        Some(&self.space_id)
    }

    /// Gate a re-authorization round trip, and report whether it may start.
    ///
    /// False while one is already out: the macOS open panel is modeless, so
    /// the window stays clickable behind it and a second press would stack a
    /// second panel over the first.
    fn begin_project_root_reauthorize(&mut self) -> bool {
        if self.reauth_in_flight {
            return false;
        }
        self.reauth_in_flight = true;
        self.interaction = InteractionState::default();
        true
    }

    fn on_project_root_reauthorize(&mut self, outcome: ProjectRootProbe) {
        self.reauth_in_flight = false;
        self.interaction = InteractionState::default();
        match outcome {
            ProjectRootProbe::Available => {
                self.other_folder_note = None;
                self.phase = ViewPhase::Granted;
            }
            ProjectRootProbe::Blocked(failure) => {
                self.root = failure.path.clone();
                self.phase = ViewPhase::Blocked(failure);
            }
            // The picker returned a different folder. The Project is not
            // repointed on the strength of that -- repointing a Project and
            // granting access to one are different acts -- so the page says
            // what happened and stays as it was.
            ProjectRootProbe::OtherFolderChosen => {
                self.other_folder_note = Some(crate::i18n::tr("project-root-other-folder"));
            }
        }
    }

    fn replace_project_root_problem(
        &mut self,
        space_id: String,
        display_name: String,
        failure: ProjectRootUnavailable,
    ) -> bool {
        self.space_id = space_id;
        self.display_name = display_name;
        self.root = failure.path.clone();
        self.phase = ViewPhase::Blocked(failure);
        self.other_folder_note = None;
        self.interaction = InteractionState::default();
        true
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

    fn on_key(
        &mut self,
        key: wezterm_term::KeyCode,
        _mods: wezterm_term::KeyModifiers,
    ) -> ContentViewResponse {
        match key {
            wezterm_term::KeyCode::Escape => ContentViewResponse::Close,
            _ => ContentViewResponse::Ignored,
        }
    }
}

/// A refusal found before any window existed to show it in: app startup
/// materializes a Space's Thread before its `TermWindow` is built. Failing
/// the window would leave the user with no window at all, so startup falls
/// back to a lenient spawn, parks the problem here, and the new window picks
/// it up.
struct PendingProjectRootProblem {
    space_id: String,
    thread_id: String,
    display_name: String,
    failure: ProjectRootUnavailable,
}

static PENDING_PROBLEMS: Mutex<Vec<PendingProjectRootProblem>> = Mutex::new(Vec::new());

pub(crate) fn queue_project_root_problem(
    space_id: String,
    thread_id: String,
    display_name: String,
    failure: ProjectRootUnavailable,
) {
    let Ok(mut pending) = PENDING_PROBLEMS.lock() else {
        return;
    };
    // One per Thread: a Space re-materialized twice must not stack up pages.
    pending.retain(|entry| entry.thread_id != thread_id);
    // Nothing drains entries for a Space no window ever claims, so the queue
    // is bounded rather than left to grow for the life of the process.
    const MAX_PARKED: usize = 8;
    if pending.len() >= MAX_PARKED {
        pending.remove(0);
    }
    pending.push(PendingProjectRootProblem {
        space_id,
        thread_id,
        display_name,
        failure,
    });
}

impl TermWindow {
    /// Show any refusal that was parked while this window was still being
    /// built. Called once, at the end of window construction.
    pub(crate) fn show_pending_project_root_problem(&mut self) {
        let taken = {
            let Ok(mut pending) = PENDING_PROBLEMS.lock() else {
                return;
            };
            // Drain everything parked for this Space; the most recent wins.
            // Removing only one would strand a second Thread's entry forever.
            let mut mine = Vec::new();
            let mut index = 0;
            while index < pending.len() {
                if pending[index].space_id == self.active_space_id {
                    mine.push(pending.remove(index));
                } else {
                    index += 1;
                }
            }
            let Some(taken) = mine.pop() else {
                return;
            };
            taken
        };
        // Space ids are recycled, so re-check the Thread still lives in this
        // Space before showing a page about it.
        if crate::workspace_threads::thread_space_id(&taken.thread_id).as_deref()
            != Some(taken.space_id.as_str())
        {
            return;
        }
        self.show_project_root_problem(
            taken.thread_id,
            taken.space_id,
            taken.display_name,
            taken.failure,
        );
    }

    /// Show the page for a Thread that could not be opened in its Project
    /// directory. Keyed on the thread: clicking the same broken Thread twice
    /// re-points the existing page instead of stacking a second one.
    pub(crate) fn show_project_root_problem(
        &mut self,
        thread_id: String,
        space_id: String,
        display_name: String,
        failure: ProjectRootUnavailable,
    ) -> ContentViewId {
        let key = format!("{PROJECT_ROOT_CONTENT_VIEW_KEY_PREFIX}{thread_id}");
        if let Some(id) = self.content_view_id_for_key(&key) {
            // A key that somehow belongs to another view type falls through
            // to open a fresh page rather than panicking.
            if self.content_view_mut_by_id(id).is_some_and(|view| {
                view.replace_project_root_problem(
                    space_id.clone(),
                    display_name.clone(),
                    failure.clone(),
                )
            }) {
                self.set_active_content_view_id(Some(id));
                self.invalidate_window();
                return id;
            }
            log::warn!("content view key {key} is not a project-root page; opening a new one");
        }
        let view = ProjectRootView::new(thread_id, space_id, display_name, failure);
        let id = self.open_content_view(Box::new(view));
        self.invalidate_window();
        id
    }

    /// Hand the folder back through the native picker, then confirm with one
    /// directory read. Every filesystem touch happens on a worker: an
    /// unresponsive mount is exactly the case this page exists for.
    pub(crate) fn reauthorize_project_root(&mut self, view_id: ContentViewId, root: PathBuf) {
        let started = self
            .content_view_mut_by_id(view_id)
            .is_some_and(|view| view.begin_project_root_reauthorize());
        if !started {
            return;
        }
        self.invalidate_window();
        let Some(window) = self.window.as_ref().cloned() else {
            self.deliver_project_root_reauthorize(
                view_id,
                ProjectRootProbe::Blocked(ProjectRootUnavailable {
                    path: root,
                    problem: FolderProblem::Other,
                    detail: "window is no longer available".to_string(),
                    via_session_server: false,
                }),
            );
            return;
        };
        let notify = window.clone();
        window.pick_folder_async_with_options(
            window::FolderPickerOptions {
                title: crate::i18n::tr("project-root-reauthorize-title"),
                prompt: crate::i18n::tr("project-root-reauthorize-prompt"),
                directory: Some(root.clone()),
            },
            Box::new(move |chosen| {
                promise::spawn::spawn(async move {
                    let outcome = promise::spawn::spawn_into_new_thread(move || {
                        Ok(reauthorize_outcome(root, chosen))
                    })
                    .await
                    .unwrap_or_else(|err| {
                        ProjectRootProbe::Blocked(ProjectRootUnavailable {
                            path: PathBuf::new(),
                            problem: FolderProblem::Other,
                            detail: format!("{err:#}"),
                            via_session_server: false,
                        })
                    });
                    notify.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                        term_window.deliver_project_root_reauthorize(view_id, outcome);
                    })));
                })
                .detach();
            }),
        );
    }

    fn deliver_project_root_reauthorize(
        &mut self,
        view_id: ContentViewId,
        outcome: ProjectRootProbe,
    ) {
        // The view may have been closed while the panel was up; there is
        // nothing to update and nothing to report.
        if let Some(view) = self.content_view_mut_by_id(view_id) {
            view.on_project_root_reauthorize(outcome);
            self.invalidate_window();
        }
    }
}

impl TermWindow {
    /// The Project menu's "Grant Folder Access…": the same picker the blocked
    /// page offers, without needing that page first. A grant is followed by
    /// opening the Project's first Thread, so the spawn (through the session
    /// server when local sessions use it) proves the access end to end.
    pub(crate) fn grant_project_folder_access(&mut self, project_id: String) {
        let Some(root) = crate::workspace_threads::local_project_path(&project_id) else {
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        let notify = window.clone();
        window.pick_folder_async_with_options(
            window::FolderPickerOptions {
                title: crate::i18n::tr("project-root-reauthorize-title"),
                prompt: crate::i18n::tr("project-root-reauthorize-prompt"),
                directory: Some(root.clone()),
            },
            Box::new(move |chosen| {
                promise::spawn::spawn(async move {
                    let outcome = promise::spawn::spawn_into_new_thread(move || {
                        Ok(reauthorize_outcome(root, chosen))
                    })
                    .await
                    .unwrap_or_else(|err| {
                        ProjectRootProbe::Blocked(ProjectRootUnavailable {
                            path: PathBuf::new(),
                            problem: FolderProblem::Other,
                            detail: format!("{err:#}"),
                            via_session_server: false,
                        })
                    });
                    notify.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                        term_window.deliver_project_folder_grant(project_id, outcome);
                    })));
                })
                .detach();
            }),
        );
    }

    fn deliver_project_folder_grant(&mut self, project_id: String, outcome: ProjectRootProbe) {
        let thread_id = crate::workspace_threads::ordered_thread_ids(&project_id)
            .into_iter()
            .next()
            .or_else(|| crate::workspace_threads::create_thread(&project_id, None).ok());
        let Some(thread_id) = thread_id else {
            self.invalidate_window();
            return;
        };
        match outcome {
            ProjectRootProbe::Available => {
                if let Some(window) = self.window.as_ref().cloned() {
                    self.activate_workspace_thread(thread_id, &window);
                }
            }
            ProjectRootProbe::Blocked(failure) => {
                let display_name =
                    crate::workspace_threads::thread_display_name(&project_id, &thread_id);
                let space_id = self.active_space_id.clone();
                self.show_project_root_problem(thread_id, space_id, display_name, failure);
            }
            ProjectRootProbe::OtherFolderChosen => {
                log::info!("grant folder access: a different folder was chosen; nothing granted");
            }
        }
        self.invalidate_window();
    }
}

/// The pane where macOS keeps per-app folder grants; the session server is
/// listed there under its own name once it has asked.
fn open_files_and_folders_settings() {
    wezterm_open_url::open_url(
        "x-apple.systempreferences:com.apple.preference.security?Privacy_FilesAndFolders",
    );
}

/// Decide what a finished picker means, on a worker thread.
///
/// Cancelling re-states the refusal rather than inventing a verdict: nothing
/// about the folder changed, so the page should look exactly as it did.
fn reauthorize_outcome(root: PathBuf, chosen: Option<PathBuf>) -> ProjectRootProbe {
    let Some(chosen) = chosen else {
        return probe_root(root);
    };
    if !same_path(&chosen, &root) {
        return ProjectRootProbe::OtherFolderChosen;
    }
    probe_root(root)
}

fn probe_root(root: PathBuf) -> ProjectRootProbe {
    match std::fs::read_dir(&root) {
        Ok(_) => ProjectRootProbe::Available,
        Err(err) => ProjectRootProbe::Blocked(ProjectRootUnavailable::from_io(root, &err)),
    }
}

/// Compare two folders for "is this the same place". Canonicalization is
/// best-effort: the folder is one the system may refuse to traverse, and a
/// refusal falls back to comparing the paths as given.
fn same_path(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Open the Thread by asking for exactly what a sidebar click asks for. The
/// page is dismissed first; a still-refused Project simply re-opens it.
fn open_thread_response(thread_id: String) -> ContentViewResponse {
    ContentViewResponse::Run(Box::new(move |tw: &mut TermWindow| {
        if let Some(view_id) = tw.content_view_response_tab_id {
            tw.close_content_view_by_id(view_id);
        }
        let Some(window) = tw.window.as_ref().cloned() else {
            return;
        };
        tw.activate_workspace_thread(thread_id, &window);
    }))
}

/// Let the user hand the folder back to ThinkTerm through the native picker.
fn reauthorize_response(root: PathBuf) -> ContentViewResponse {
    ContentViewResponse::Run(Box::new(move |tw: &mut TermWindow| {
        let Some(view_id) = tw.content_view_response_tab_id else {
            return;
        };
        tw.reauthorize_project_root(view_id, root);
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(path: &str) -> ProjectRootUnavailable {
        ProjectRootUnavailable {
            path: PathBuf::from(path),
            problem: FolderProblem::UnreadableRoot,
            detail: "denied".to_string(),
            via_session_server: false,
        }
    }

    fn view() -> ProjectRootView {
        ProjectRootView::new(
            "thread-a".to_string(),
            "space-a".to_string(),
            "Project / Thread".to_string(),
            blocked("/project"),
        )
    }

    /// The macOS open panel is modeless, so the window stays clickable while
    /// it is up. A second press must not stack a second panel.
    #[test]
    fn only_one_reauthorization_may_be_in_flight() {
        let mut view = view();
        assert!(view.begin_project_root_reauthorize());
        assert!(!view.begin_project_root_reauthorize());
        assert!(
            view.buttons().is_empty(),
            "the button must not be offered while a panel is up"
        );

        view.on_project_root_reauthorize(ProjectRootProbe::Available);
        assert!(matches!(view.phase, ViewPhase::Granted));
        assert!(view.begin_project_root_reauthorize());
    }

    /// A refusal that arrives after the picker replaces the classification and
    /// the path, so a folder that changed its failure mode is reported as it
    /// now is.
    #[test]
    fn a_fresh_refusal_replaces_the_old_one() {
        let mut view = view();
        assert!(view.begin_project_root_reauthorize());
        view.on_project_root_reauthorize(ProjectRootProbe::Blocked(ProjectRootUnavailable {
            path: PathBuf::from("/project"),
            problem: FolderProblem::MissingRoot,
            detail: "gone".to_string(),
            via_session_server: false,
        }));
        assert!(matches!(&view.phase, ViewPhase::Blocked(failure)
                if failure.problem == FolderProblem::MissingRoot));
    }

    /// Picking somewhere else never repoints the Project; it says so and
    /// leaves the refusal on screen.
    #[test]
    fn another_folder_is_reported_not_adopted() {
        let mut view = view();
        assert!(view.begin_project_root_reauthorize());
        view.on_project_root_reauthorize(ProjectRootProbe::OtherFolderChosen);
        assert!(matches!(view.phase, ViewPhase::Blocked(_)));
        assert_eq!(view.root, PathBuf::from("/project"));
        assert!(view.other_folder_note.is_some());
    }

    /// Cancelling the panel is not a verdict: the page must look exactly as
    /// it did, not claim the folder became readable.
    #[test]
    fn cancelling_the_picker_restates_the_refusal() {
        let outcome = super::reauthorize_outcome(PathBuf::from("/nope/missing"), None);
        assert!(matches!(outcome, ProjectRootProbe::Blocked(_)));
    }

    #[test]
    fn deduplicated_problem_uses_the_latest_context() {
        let mut view = view();
        assert!(view.replace_project_root_problem(
            "space-b".to_string(),
            "New name".to_string(),
            blocked("/new-project"),
        ));
        assert_eq!(view.space_id, "space-b");
        assert_eq!(view.display_name, "New name");
        assert_eq!(view.root, PathBuf::from("/new-project"));
        assert!(matches!(view.phase, ViewPhase::Blocked(_)));
    }
}
