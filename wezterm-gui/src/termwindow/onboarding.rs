use crate::native_settings::{
    mark_onboarding_seen, NativeLanguagePreference, NativeThemeMode, ThinkTermNativeSettings,
};
use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::content_view::{ContentView, ContentViewResponse};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::TermWindow;
use crate::ui::anim::Easing;
use crate::ui::{
    draw_scrollbar, draw_toggle, rect, wheel_delta_pixels, ControlState, DrawContext,
    InteractionState, ScrollState, UiContext, UiPalette, UiTokens, WidgetKind,
};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};
use wezterm_font::LoadedFont;
use wezterm_term::{KeyCode, KeyModifiers};
use window::color::LinearRgba;
use window::{MouseEventKind as WMEK, MousePress, RectF, WindowOps};

const OUTER_PAD: f32 = 56.0;
const CARD_RADIUS: f32 = 20.0;
const ACTION_H: f32 = 66.0;
const CONTENT_MAX_W: f32 = 1240.0;
const HEADER_H: f32 = 160.0;
const FOOTER_H: f32 = 150.0;
const PROJECT_ROW_H: f32 = 118.0;
const PROJECT_ROW_GAP: f32 = 10.0;
const TRANSITION_MS: u64 = 280;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Welcome,
    ImportSources,
    ReviewProjects,
    Preferences,
    Ready,
}

impl Step {
    fn all() -> &'static [Step] {
        &[
            Self::Welcome,
            Self::ImportSources,
            Self::ReviewProjects,
            Self::Preferences,
            Self::Ready,
        ]
    }

    fn index(self) -> usize {
        Self::all()
            .iter()
            .position(|step| *step == self)
            .unwrap_or(0)
    }

    fn title(self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::ImportSources => "Import Sources",
            Self::ReviewProjects => "Review Projects",
            Self::Preferences => "Preferences",
            Self::Ready => "Ready",
        }
    }

    fn eyebrow(self) -> &'static str {
        match self {
            Self::Welcome => "Set up your native terminal workspace",
            Self::ImportSources => "Choose where projects come from",
            Self::ReviewProjects => "Confirm before importing",
            Self::Preferences => "Tune the native workspace",
            Self::Ready => "Start clean",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Welcome => Self::ImportSources,
            Self::ImportSources => Self::ReviewProjects,
            Self::ReviewProjects => Self::Preferences,
            Self::Preferences => Self::Ready,
            Self::Ready => Self::Ready,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Welcome => Self::Welcome,
            Self::ImportSources => Self::Welcome,
            Self::ReviewProjects => Self::ImportSources,
            Self::Preferences => Self::ReviewProjects,
            Self::Ready => Self::Preferences,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ImportProvider {
    VsCode,
    Cursor,
    Antigravity,
    Cmux,
    Manual,
}

impl ImportProvider {
    fn all() -> &'static [Self] {
        &[
            Self::VsCode,
            Self::Cursor,
            Self::Antigravity,
            Self::Cmux,
            Self::Manual,
        ]
    }

    fn label(self) -> &'static str {
        match self {
            Self::VsCode => "VS Code",
            Self::Cursor => "Cursor",
            Self::Antigravity => "Antigravity",
            Self::Cmux => "cmux",
            Self::Manual => "Choose Folder Manually",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::VsCode => "Workspace scan is not available in v1.",
            Self::Cursor => "Cursor project history import is coming soon.",
            Self::Antigravity => "Local Antigravity scan is not yet supported.",
            Self::Cmux => "cmux session discovery is not wired yet.",
            Self::Manual => "Pick a folder and review it before import.",
        }
    }

    fn icon(self) -> SvgIcon {
        match self {
            Self::VsCode | Self::Cursor => SvgIcon::SquareTerminal,
            Self::Antigravity => SvgIcon::Layers,
            Self::Cmux => SvgIcon::Terminal,
            Self::Manual => SvgIcon::FolderPlus,
        }
    }

    fn is_available(self) -> bool {
        matches!(self, Self::Manual)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum OnboardingAction {
    Primary,
    Back,
    Skip,
    Provider(ImportProvider),
    ToggleProject(usize),
    RemoveProject(usize),
    Language(NativeLanguagePreference),
    Appearance(NativeThemeMode),
    ToggleSidebar,
}

#[derive(Debug, Clone, Copy)]
struct StepTransition {
    from: Step,
    to: Step,
    started_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectSource {
    Manual,
}

impl ProjectSource {
    fn label(&self) -> &'static str {
        match self {
            Self::Manual => "Manual",
        }
    }
}

#[derive(Debug, Clone)]
struct PendingProject {
    name: String,
    path: PathBuf,
    source: ProjectSource,
    selected: bool,
}

impl PendingProject {
    fn from_path(path: PathBuf, source: ProjectSource) -> Self {
        let name = project_name_for_path(&path);
        Self {
            name,
            path,
            source,
            selected: true,
        }
    }
}

pub(crate) struct OnboardingView {
    /// UI scale captured at paint time; mouse handlers get no `DrawContext`.
    last_ui_scale: f32,
    step: Step,
    widgets: UiContext<OnboardingAction>,
    interaction: InteractionState<OnboardingAction>,
    review_scroll: ScrollState,
    initial_space_id: String,
    initial_space_name: String,
    pending_projects: Vec<PendingProject>,
    target_space_id: Option<String>,
    imported_project_count: usize,
    selected_language: NativeLanguagePreference,
    selected_appearance: NativeThemeMode,
    show_left_sidebar: bool,
    status: Option<String>,
    transition: Option<StepTransition>,
}

impl OnboardingView {
    pub(crate) fn new(initial_space_id: String, initial_space_name: String) -> Self {
        let settings = crate::native_settings::load();
        Self {
            step: Step::Welcome,
            widgets: UiContext::default(),
            interaction: InteractionState::default(),
            review_scroll: ScrollState::new(),
            initial_space_id,
            initial_space_name,
            pending_projects: Vec::new(),
            target_space_id: None,
            imported_project_count: 0,
            selected_language: settings.onboarding.language,
            selected_appearance: settings.appearance.theme_mode,
            show_left_sidebar: settings.onboarding.show_left_sidebar_by_default,
            status: None,
            transition: None,
            last_ui_scale: 1.0,
        }
    }

    fn normalized_space_name(&self) -> String {
        self.fallback_space_name()
    }

    fn fallback_space_name(&self) -> String {
        let initial_name = self.initial_space_name.trim();
        if !initial_name.is_empty() {
            initial_name.to_string()
        } else {
            let initial_id = self.initial_space_id.trim();
            if initial_id.is_empty() {
                "Default".to_string()
            } else {
                initial_id.to_string()
            }
        }
    }

    fn selected_project_count(&self) -> usize {
        self.pending_projects
            .iter()
            .filter(|project| project.selected)
            .count()
    }

    fn start_step_transition(&mut self, next: Step, _direction: f32) {
        let from = self.step;
        if from == next {
            return;
        }
        self.step = next;
        if next == Step::ReviewProjects {
            self.review_scroll.reset();
        }
        self.interaction.pressed = None;
        self.transition = Some(StepTransition {
            from,
            to: next,
            started_at: Instant::now(),
        });
    }

    fn target_space_id_for_choice(&self) -> String {
        let initial_id = self.initial_space_id.trim();
        if initial_id.is_empty() {
            crate::workspace_threads::ensure_space_named(&self.normalized_space_name())
        } else {
            self.initial_space_id.clone()
        }
    }

    fn primary_label(&self) -> &'static str {
        match self.step {
            Step::Welcome | Step::ImportSources | Step::Preferences => "Continue",
            Step::ReviewProjects => {
                if self.selected_project_count() == 0 {
                    "Continue Without Import"
                } else {
                    "Import Selected"
                }
            }
            Step::Ready => "Start ThinkTerm",
        }
    }

    fn primary_enabled(&self) -> bool {
        true
    }

    fn on_mouse_impl(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse {
        let hit = self.widgets.hit_test(x, y).map(|target| target.action);
        match kind {
            WMEK::VertWheel(amount) if self.step == Step::ReviewProjects => {
                let old = self.review_scroll.offset;
                self.review_scroll
                    .scroll_by(wheel_delta_pixels(amount, self.last_ui_scale));
                if (self.review_scroll.offset - old).abs() > 0.01 {
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
                if let (Some(a), Some(b)) = (hit, pressed) {
                    if a == b {
                        return self.apply(a);
                    }
                }
                ContentViewResponse::Redraw
            }
            _ => ContentViewResponse::Ignored,
        }
    }

    fn apply(&mut self, action: OnboardingAction) -> ContentViewResponse {
        self.status = None;
        match action {
            OnboardingAction::Primary => self.apply_primary(),
            OnboardingAction::Back => {
                self.start_step_transition(self.step.previous(), -1.0);
                ContentViewResponse::Redraw
            }
            OnboardingAction::Skip => self.skip_response(),
            OnboardingAction::Provider(provider) => {
                if provider.is_available() {
                    ContentViewResponse::Run(Box::new(|tw: &mut TermWindow| {
                        tw.pick_content_view_folder();
                    }))
                } else {
                    self.status = Some(format!("{} import is coming soon.", provider.label()));
                    ContentViewResponse::Redraw
                }
            }
            OnboardingAction::ToggleProject(index) => {
                if let Some(project) = self.pending_projects.get_mut(index) {
                    project.selected = !project.selected;
                }
                ContentViewResponse::Redraw
            }
            OnboardingAction::RemoveProject(index) => {
                if index < self.pending_projects.len() {
                    self.pending_projects.remove(index);
                }
                ContentViewResponse::Redraw
            }
            OnboardingAction::Language(language) => {
                self.selected_language = language;
                ContentViewResponse::Redraw
            }
            OnboardingAction::Appearance(mode) => {
                self.selected_appearance = mode;
                ContentViewResponse::Redraw
            }
            OnboardingAction::ToggleSidebar => {
                self.show_left_sidebar = !self.show_left_sidebar;
                ContentViewResponse::Redraw
            }
        }
    }

    fn apply_primary(&mut self) -> ContentViewResponse {
        if !self.primary_enabled() {
            return ContentViewResponse::Redraw;
        }
        match self.step {
            Step::Welcome | Step::ImportSources => {
                self.start_step_transition(self.step.next(), 1.0);
                ContentViewResponse::Redraw
            }
            Step::ReviewProjects => {
                self.import_selected_projects();
                self.start_step_transition(Step::Preferences, 1.0);
                ContentViewResponse::Redraw
            }
            Step::Preferences => {
                self.start_step_transition(Step::Ready, 1.0);
                ContentViewResponse::Redraw
            }
            Step::Ready => self.finish_response(),
        }
    }

    fn import_selected_projects(&mut self) {
        let space_id = self
            .target_space_id
            .clone()
            .unwrap_or_else(|| self.target_space_id_for_choice());
        let mut imported = 0usize;
        for project in self
            .pending_projects
            .iter()
            .filter(|project| project.selected)
        {
            let path = project.path.to_string_lossy();
            match crate::workspace_threads::create_project_from_path(&space_id, path.as_ref()) {
                Ok(_) => imported += 1,
                Err(err) => {
                    log::error!(
                        "failed to import onboarding project {}: {err:#}",
                        project.path.display()
                    );
                }
            }
        }
        self.target_space_id = Some(space_id);
        self.imported_project_count = imported;
    }

    fn finish_response(&self) -> ContentViewResponse {
        let prefs = OnboardingPrefs {
            space_name: self.normalized_space_name(),
            target_space_id: Some(
                self.target_space_id
                    .clone()
                    .unwrap_or_else(|| self.target_space_id_for_choice()),
            ),
            imported_project_count: self.imported_project_count,
            language: self.selected_language,
            appearance: self.selected_appearance,
            show_left_sidebar: self.show_left_sidebar,
        };
        ContentViewResponse::Run(Box::new(move |tw: &mut TermWindow| {
            finish_onboarding(tw, prefs);
        }))
    }

    fn skip_response(&self) -> ContentViewResponse {
        ContentViewResponse::Run(Box::new(|tw: &mut TermWindow| {
            let mut settings = crate::native_settings::load();
            mark_onboarding_seen(&mut settings);
            if let Err(err) = crate::native_settings::save(&settings) {
                log::error!("failed to save onboarding skip state: {err:#}");
            }
            tw.close_content_view();
        }))
    }

    fn on_key_impl(&mut self, key: KeyCode, mods: KeyModifiers) -> ContentViewResponse {
        match (key, mods) {
            (KeyCode::Escape, _) => self.skip_response(),
            (KeyCode::Enter, _) => self.apply_primary(),
            (KeyCode::Tab, KeyModifiers::NONE) => {
                self.start_step_transition(self.step.next(), 1.0);
                ContentViewResponse::Redraw
            }
            (KeyCode::Tab, KeyModifiers::SHIFT) => {
                self.start_step_transition(self.step.previous(), -1.0);
                ContentViewResponse::Redraw
            }
            _ => ContentViewResponse::Ignored,
        }
    }

    fn on_paste_impl(&mut self, _text: &str) -> ContentViewResponse {
        ContentViewResponse::Ignored
    }

    fn add_manual_project(&mut self, path: PathBuf) {
        if self
            .pending_projects
            .iter()
            .any(|project| project.path == path)
        {
            self.status = Some("That folder is already in the review list.".to_string());
        } else {
            self.pending_projects
                .push(PendingProject::from_path(path, ProjectSource::Manual));
            self.status = Some("Folder added. Review it before importing.".to_string());
        }
        self.start_step_transition(Step::ReviewProjects, 1.0);
        self.review_scroll.reset();
    }

    fn line_h(ctx: &DrawContext) -> f32 {
        (ctx.metrics.cell_size.height as f32 + ctx.px(14.0)).max(ctx.px(34.0))
    }

    fn compact_line_h(ctx: &DrawContext) -> f32 {
        (ctx.metrics.cell_size.height as f32 + ctx.px(8.0)).max(ctx.px(28.0))
    }

    fn control_h(ctx: &DrawContext) -> f32 {
        (ctx.metrics.cell_size.height as f32 + ctx.px(30.0)).max(ctx.px(60.0))
    }

    fn preference_choice_h(ctx: &DrawContext) -> f32 {
        (Self::control_h(ctx) + ctx.px(10.0)).clamp(ctx.px(68.0), ctx.px(80.0))
    }

    fn preference_group_height(ctx: &DrawContext, choice_count: usize) -> f32 {
        let rows = choice_count.div_ceil(2).max(1) as f32;
        Self::line_h(ctx)
            + ctx.px(26.0)
            + rows * Self::preference_choice_h(ctx)
            + (rows - 1.0) * ctx.px(18.0)
    }

    fn action_h(ctx: &DrawContext) -> f32 {
        Self::control_h(ctx).min(ctx.px(68.0)).max(ctx.px(ACTION_H))
    }

    fn centered_text_y(ctx: &DrawContext, area: RectF) -> f32 {
        let text_h = ctx.metrics.cell_size.height as f32;
        area.origin.y + ((area.size.height - text_h) / 2.0).max(ctx.px(4.0))
    }

    fn transition_duration() -> Duration {
        Duration::from_millis(TRANSITION_MS)
    }

    fn transition_t(&self) -> Option<f32> {
        self.transition.map(|transition| {
            let elapsed = transition.started_at.elapsed();
            (elapsed.as_secs_f32() / Self::transition_duration().as_secs_f32()).clamp(0.0, 1.0)
        })
    }

    fn paint_body_for_step(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        step: Step,
        body: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        tokens: UiTokens,
        section_font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        match step {
            Step::Welcome => self.paint_welcome(ctx, layers, body, palette, font)?,
            Step::ImportSources => self.paint_import_sources(ctx, layers, body, palette, font)?,
            Step::ReviewProjects => self.paint_review(ctx, layers, body, palette, font, tokens)?,
            Step::Preferences => {
                self.paint_preferences(ctx, layers, body, palette, font, section_font)?
            }
            Step::Ready => self.paint_ready(ctx, layers, body, palette, font)?,
        }
        Ok(())
    }

    fn paint_body(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        body: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        tokens: UiTokens,
        section_font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        if self.transition_t().is_some_and(|raw_t| raw_t >= 1.0) {
            self.transition = None;
        }

        self.paint_body_for_step(
            ctx,
            layers,
            self.step,
            body,
            palette,
            font,
            tokens,
            section_font,
        )
    }

    fn paint_impl(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        _cursor_on: bool,
    ) -> anyhow::Result<()> {
        self.widgets.clear();
        self.last_ui_scale = ctx.scale();
        let tokens = UiTokens::for_dpi(ctx.dimensions.dpi);
        ctx.draw_rect(
            layers,
            0,
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            palette.window_bg,
        )?;

        let shell = inset_rect(area, ctx.px(OUTER_PAD));
        let content_w = shell.size.width.min(ctx.px(CONTENT_MAX_W)).max(0.0);
        let content_x = shell.origin.x + ((shell.size.width - content_w) / 2.0).max(0.0);
        let content = rect(content_x, shell.origin.y, content_w, shell.size.height);

        self.paint_header(ctx, layers, content, palette, font, title_font)?;
        let body = rect(
            content.origin.x,
            content.origin.y + ctx.px(HEADER_H),
            content.size.width,
            (content.size.height - ctx.px(HEADER_H + FOOTER_H)).max(0.0),
        );
        self.paint_body(ctx, layers, body, palette, font, tokens, section_font)?;
        self.paint_status(ctx, layers, content, palette, font)?;
        self.paint_footer(ctx, layers, content, palette, font)?;
        Ok(())
    }

    fn paint_header(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        content: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        ctx.draw_text(
            layers,
            title_font,
            content.origin.x,
            content.origin.y + ctx.px(6.0),
            self.step.title(),
            palette.text,
            content.size.width,
        )?;
        self.draw_wrapped_text(
            ctx,
            layers,
            font,
            content.origin.x,
            content.origin.y + ctx.px(64.0),
            content.size.width.min(ctx.px(900.0)),
            self.step.eyebrow(),
            palette.muted_text,
            2,
        )?;
        ctx.draw_rect(
            layers,
            0,
            content.origin.x,
            content.origin.y + ctx.px(HEADER_H - 24.0),
            content.size.width,
            1.0,
            palette.separator,
        )
    }

    fn paint_welcome(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        self.draw_wrapped_text(
            ctx,
            layers,
            font,
            area.origin.x,
            area.origin.y,
            area.size.width.min(ctx.px(960.0)),
            "Open folders, review imports, and keep terminal sessions tied to the work they belong to.",
            palette.secondary_text,
            2,
        )?;
        let line_h = Self::line_h(ctx);
        let card_h = (line_h * 3.2 + ctx.px(58.0)).max(ctx.px(156.0));
        let cards = [
            (
                SvgIcon::Layers,
                "Workspaces",
                "Keep related folders and sessions together.",
            ),
            (
                SvgIcon::FolderOpen,
                "Projects",
                "Review each folder before ThinkTerm creates it.",
            ),
            (
                SvgIcon::SquareTerminal,
                "Sessions",
                "Return to long-running terminal work quickly.",
            ),
        ];
        let card_gap = ctx.px(22.0);
        let card_w = ((area.size.width - card_gap * 2.0) / 3.0).max(ctx.px(240.0));
        let mut x = area.origin.x;
        let y = area.origin.y + line_h * 2.0 + ctx.px(46.0);
        for (icon, title, description) in cards {
            self.paint_info_card(
                ctx,
                layers,
                rect(x, y, card_w, card_h),
                palette,
                font,
                icon,
                title,
                description,
            )?;
            x += card_w + card_gap;
        }
        self.paint_info_card(
            ctx,
            layers,
            rect(
                area.origin.x,
                y + card_h + ctx.px(24.0),
                area.size.width,
                (line_h * 2.6 + ctx.px(54.0)).max(ctx.px(136.0)),
            ),
            palette,
            font,
            SvgIcon::Info,
            "Private by default",
            "No project paths, terminal content, or local folders are collected. Manual imports stay local until you confirm.",
        )?;
        Ok(())
    }

    fn paint_import_sources(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        self.draw_wrapped_text(
            ctx,
            layers,
            font,
            area.origin.x,
            area.origin.y,
            area.size.width.min(ctx.px(960.0)),
            "Choose a source to scan. v1 imports manual folders only, and nothing is created until review.",
            palette.secondary_text,
            2,
        )?;
        let line_h = Self::line_h(ctx);
        let grid_w = area.size.width;
        let gap = 20.0;
        let card_w = ((grid_w - gap) / 2.0).max(ctx.px(280.0));
        let card_h = (line_h * 3.0 + ctx.px(64.0)).max(ctx.px(164.0));
        let start_y = area.origin.y + line_h * 2.0 + ctx.px(42.0);
        for (idx, provider) in ImportProvider::all().iter().enumerate() {
            let is_manual = *provider == ImportProvider::Manual;
            let row = idx / 2;
            let col = idx % 2;
            let x = area.origin.x + col as f32 * (card_w + gap);
            let y = start_y + row as f32 * (card_h + gap);
            self.paint_provider_card(
                ctx,
                layers,
                rect(
                    if is_manual { area.origin.x } else { x },
                    y,
                    if is_manual { grid_w } else { card_w },
                    card_h,
                ),
                palette,
                font,
                *provider,
            )?;
        }
        Ok(())
    }

    fn paint_review(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        tokens: UiTokens,
    ) -> anyhow::Result<()> {
        let count = self.pending_projects.len();
        let selected = self.selected_project_count();
        let summary = format!("{selected} of {count} projects selected");
        let line_h = Self::line_h(ctx);
        ctx.draw_text(
            layers,
            font,
            area.origin.x,
            area.origin.y,
            &summary,
            palette.secondary_text,
            area.size.width,
        )?;
        let list = rect(
            area.origin.x,
            area.origin.y + line_h + 20.0,
            area.size.width.min(ctx.px(820.0)),
            (area.size.height - line_h - 28.0).max(0.0),
        );
        if self.pending_projects.is_empty() {
            self.paint_empty_state(ctx, layers, list, palette, font)?;
            return Ok(());
        }
        let content_h = self.pending_projects.len() as f32 * ctx.px(PROJECT_ROW_H)
            + self.pending_projects.len().saturating_sub(1) as f32 * ctx.px(PROJECT_ROW_GAP);
        self.review_scroll.set_extents(list.size.height, content_h);
        let mut y = list.origin.y - self.review_scroll.offset;
        let projects = self.pending_projects.clone();
        for (idx, project) in projects.iter().enumerate() {
            if y + ctx.px(PROJECT_ROW_H) >= list.origin.y && y <= list.origin.y + list.size.height {
                self.paint_project_row(
                    ctx,
                    layers,
                    rect(list.origin.x, y, list.size.width, ctx.px(PROJECT_ROW_H)),
                    list,
                    palette,
                    font,
                    idx,
                    project,
                )?;
            }
            y += ctx.px(PROJECT_ROW_H + PROJECT_ROW_GAP);
        }
        if self.review_scroll.has_overflow() {
            draw_scrollbar(ctx, layers, palette, tokens, list, self.review_scroll)?;
        }
        Ok(())
    }

    fn paint_preferences(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        let group_w = area.size.width.min(ctx.px(1120.0));
        let language_h = Self::preference_group_height(ctx, 4);
        self.paint_preference_group(
            ctx,
            layers,
            rect(area.origin.x, area.origin.y, group_w, language_h),
            palette,
            font,
            section_font,
            "Language",
            &[
                (
                    NativeLanguagePreference::System.label(),
                    OnboardingAction::Language(NativeLanguagePreference::System),
                    self.selected_language == NativeLanguagePreference::System,
                ),
                (
                    NativeLanguagePreference::English.label(),
                    OnboardingAction::Language(NativeLanguagePreference::English),
                    self.selected_language == NativeLanguagePreference::English,
                ),
                (
                    NativeLanguagePreference::Chinese.label(),
                    OnboardingAction::Language(NativeLanguagePreference::Chinese),
                    self.selected_language == NativeLanguagePreference::Chinese,
                ),
                (
                    NativeLanguagePreference::Japanese.label(),
                    OnboardingAction::Language(NativeLanguagePreference::Japanese),
                    self.selected_language == NativeLanguagePreference::Japanese,
                ),
            ],
        )?;
        let appearance_y = area.origin.y + language_h + ctx.px(54.0);
        let appearance_h = Self::preference_group_height(ctx, 3);
        self.paint_preference_group(
            ctx,
            layers,
            rect(area.origin.x, appearance_y, group_w, appearance_h),
            palette,
            font,
            section_font,
            "Appearance",
            &[
                (
                    "Follow System",
                    OnboardingAction::Appearance(NativeThemeMode::System),
                    self.selected_appearance == NativeThemeMode::System,
                ),
                (
                    "Light",
                    OnboardingAction::Appearance(NativeThemeMode::Light),
                    self.selected_appearance == NativeThemeMode::Light,
                ),
                (
                    "Dark",
                    OnboardingAction::Appearance(NativeThemeMode::Dark),
                    self.selected_appearance == NativeThemeMode::Dark,
                ),
            ],
        )?;
        let toggle_y = appearance_y + appearance_h + ctx.px(58.0);
        let toggle_row = rect(area.origin.x, toggle_y, group_w, ctx.px(124.0));
        self.widgets.push(
            toggle_row,
            WidgetKind::Button,
            OnboardingAction::ToggleSidebar,
        );
        let hovered = self.interaction.hovered == Some(OnboardingAction::ToggleSidebar);
        let pressed = self.interaction.pressed == Some(OnboardingAction::ToggleSidebar);
        let toggle_bg = if pressed {
            palette.control_pressed_bg
        } else if hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        ctx.draw_rounded_frame(
            layers,
            0,
            toggle_row.origin.x,
            toggle_row.origin.y,
            toggle_row.size.width,
            toggle_row.size.height,
            toggle_bg,
            palette.control_border,
            ctx.px(CARD_RADIUS),
        )?;
        let toggle_rect = rect(
            toggle_row.origin.x + ctx.px(24.0),
            toggle_row.origin.y + ctx.px(47.0),
            ctx.px(54.0),
            ctx.px(30.0),
        );
        draw_toggle(
            ctx,
            layers,
            &mut self.widgets,
            palette,
            toggle_rect,
            self.show_left_sidebar,
            OnboardingAction::ToggleSidebar,
        )?;
        ctx.draw_text(
            layers,
            section_font,
            toggle_row.origin.x + 100.0,
            toggle_row.origin.y + ctx.px(26.0),
            "Show left sidebar by default",
            palette.text,
            toggle_row.size.width - ctx.px(124.0),
        )?;
        ctx.draw_text(
            layers,
            font,
            toggle_row.origin.x + 100.0,
            toggle_row.origin.y + ctx.px(72.0),
            "Applies to newly opened main windows and this setup finish.",
            palette.muted_text,
            toggle_row.size.width - ctx.px(124.0),
        )?;
        Ok(())
    }

    fn paint_ready(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        let rows = [
            ("Space", self.normalized_space_name()),
            ("Projects imported", self.imported_project_count.to_string()),
            (
                "Sidebar",
                if self.show_left_sidebar {
                    "Shown by default".to_string()
                } else {
                    "Hidden by default".to_string()
                },
            ),
            ("Language", self.selected_language.label().to_string()),
            ("Appearance", self.selected_appearance.label().to_string()),
        ];
        let mut y = area.origin.y;
        let row_h = (Self::line_h(ctx) + ctx.px(34.0)).max(ctx.px(64.0));
        for (label, value) in rows {
            self.paint_summary_row(
                ctx,
                layers,
                rect(area.origin.x, y, area.size.width.min(ctx.px(760.0)), row_h),
                palette,
                font,
                label,
                &value,
            )?;
            y += row_h + ctx.px(12.0);
        }
        Ok(())
    }

    fn paint_footer(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        content: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        let footer_y = content.origin.y + content.size.height - ctx.px(FOOTER_H);
        ctx.draw_rect(
            layers,
            0,
            content.origin.x,
            footer_y,
            content.size.width,
            1.0,
            palette.separator,
        )?;
        self.paint_step_progress(
            ctx,
            layers,
            rect(content.origin.x, footer_y + 18.0, content.size.width, 40.0),
            palette,
            font,
        )?;

        let action_h = Self::action_h(ctx);
        let y = footer_y + ctx.px(76.0);
        let primary_w = if self.step == Step::ReviewProjects {
            ctx.px(330.0)
        } else if self.step == Step::Ready {
            ctx.px(290.0)
        } else {
            ctx.px(230.0)
        };
        let primary = rect(
            content.origin.x + content.size.width - primary_w,
            y,
            primary_w,
            action_h,
        );
        self.paint_button(
            ctx,
            layers,
            primary,
            palette,
            font,
            self.primary_label(),
            OnboardingAction::Primary,
            true,
            self.primary_enabled(),
        )?;
        if self.step != Step::Welcome {
            self.paint_button(
                ctx,
                layers,
                rect(primary.origin.x - 194.0, y, 170.0, action_h),
                palette,
                font,
                "Back",
                OnboardingAction::Back,
                false,
                true,
            )?;
        }
        self.paint_button(
            ctx,
            layers,
            rect(content.origin.x, y, ctx.px(154.0), action_h),
            palette,
            font,
            "Skip",
            OnboardingAction::Skip,
            false,
            true,
        )
    }

    fn paint_status(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        content: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        let Some(status) = &self.status else {
            return Ok(());
        };
        ctx.draw_text(
            layers,
            font,
            content.origin.x,
            content.origin.y + content.size.height
                - ctx.px(FOOTER_H)
                - Self::compact_line_h(ctx)
                - ctx.px(16.0),
            status,
            palette.muted_text,
            content.size.width,
        )
    }

    fn paint_step_progress(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        let steps = Step::all();
        let progress_index =
            if let (Some(transition), Some(raw_t)) = (self.transition, self.transition_t()) {
                let from = transition.from.index() as f32;
                let to = transition.to.index() as f32;
                from + (to - from) * Easing::Smooth.apply(raw_t)
            } else {
                self.step.index() as f32
            };
        let label = format!(
            "Step {} of {} · {}",
            self.step.index() + 1,
            steps.len(),
            self.step.title()
        );
        ctx.draw_text(
            layers,
            font,
            area.origin.x,
            Self::centered_text_y(ctx, area),
            &label,
            palette.secondary_text,
            area.size.width * 0.42,
        )?;

        let track_w = (area.size.width * 0.46)
            .min(ctx.px(520.0))
            .max(ctx.px(280.0));
        let track_x = area.origin.x + area.size.width - track_w;
        let center_y = area.origin.y + area.size.height / 2.0;
        let segment_w = track_w / steps.len().max(1) as f32;
        let active_progress = palette.secondary_text;
        ctx.draw_rounded_rect(
            layers,
            0,
            track_x,
            center_y - ctx.px(2.0),
            track_w,
            ctx.px(4.0),
            palette.control_border,
            ctx.px(2.0),
        )?;
        let fill_w = segment_w * (progress_index + 1.0);
        ctx.draw_rounded_rect(
            layers,
            1,
            track_x,
            center_y - ctx.px(2.0),
            fill_w.min(track_w),
            ctx.px(4.0),
            active_progress,
            ctx.px(2.0),
        )?;
        for (idx, step) in steps.iter().enumerate() {
            let active = *step == self.step;
            let idx_f = idx as f32;
            let complete = idx_f < progress_index.floor();
            let active_strength = (1.0 - (idx_f - progress_index).abs()).clamp(0.0, 1.0);
            let cx = track_x + segment_w * idx as f32 + segment_w / 2.0;
            let size = ctx.px(14.0) + ctx.px(4.0) * active_strength;
            let color = if complete || active || active_strength > 0.01 {
                active_progress
            } else {
                palette.control_border
            };
            ctx.draw_rounded_rect(
                layers,
                1,
                cx - size / 2.0,
                center_y - size / 2.0,
                size,
                size,
                color,
                size / 2.0,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_info_card(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        card: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        icon: SvgIcon,
        title: &str,
        description: &str,
    ) -> anyhow::Result<()> {
        ctx.draw_rounded_frame(
            layers,
            0,
            card.origin.x,
            card.origin.y,
            card.size.width,
            card.size.height,
            palette.control_bg,
            palette.control_border,
            ctx.px(CARD_RADIUS),
        )?;
        let line_h = Self::line_h(ctx);
        let icon_y = card.origin.y + ctx.px(24.0);
        ctx.draw_svg_icon(
            layers,
            icon,
            card.origin.x + ctx.px(24.0),
            icon_y,
            ctx.px(28.0),
            palette.secondary_text,
        )?;
        ctx.draw_text(
            layers,
            font,
            card.origin.x + ctx.px(68.0),
            card.origin.y + ctx.px(22.0),
            title,
            palette.text,
            card.size.width - ctx.px(92.0),
        )?;
        self.draw_wrapped_text(
            ctx,
            layers,
            font,
            card.origin.x + ctx.px(24.0),
            card.origin.y + ctx.px(26.0) + line_h,
            card.size.width - ctx.px(48.0),
            description,
            palette.secondary_text,
            2,
        )
    }

    fn paint_provider_card(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        card: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        provider: ImportProvider,
    ) -> anyhow::Result<()> {
        self.widgets.push(
            card,
            WidgetKind::Button,
            OnboardingAction::Provider(provider),
        );
        let hovered = self.interaction.hovered == Some(OnboardingAction::Provider(provider));
        let pressed = self.interaction.pressed == Some(OnboardingAction::Provider(provider));
        let available = provider.is_available();
        let bg = if pressed {
            palette.control_pressed_bg
        } else if hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if hovered {
            palette.separator
        } else {
            palette.control_border
        };
        let line_h = Self::line_h(ctx);
        ctx.draw_rounded_frame(
            layers,
            0,
            card.origin.x,
            card.origin.y,
            card.size.width,
            card.size.height,
            bg,
            border,
            ctx.px(CARD_RADIUS),
        )?;
        let icon_color = if available {
            palette.secondary_text
        } else {
            palette.muted_text
        };
        ctx.draw_svg_icon(
            layers,
            provider.icon(),
            card.origin.x + ctx.px(18.0),
            card.origin.y + ctx.px(22.0),
            ctx.px(28.0),
            icon_color,
        )?;
        ctx.draw_text(
            layers,
            font,
            card.origin.x + 60.0,
            card.origin.y + ctx.px(16.0),
            provider.label(),
            palette.text,
            card.size.width - ctx.px(78.0),
        )?;
        self.draw_wrapped_text(
            ctx,
            layers,
            font,
            card.origin.x + 60.0,
            card.origin.y + ctx.px(16.0) + line_h,
            card.size.width - ctx.px(78.0),
            provider.description(),
            palette.muted_text,
            2,
        )?;
        let badge = if available {
            "Available"
        } else {
            "Coming soon"
        };
        ctx.draw_text(
            layers,
            font,
            card.origin.x + 60.0,
            card.origin.y + card.size.height - line_h - ctx.px(12.0),
            badge,
            if available {
                palette.secondary_text
            } else {
                palette.muted_text
            },
            card.size.width - ctx.px(78.0),
        )
    }

    fn paint_project_row(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        row: RectF,
        clip: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        index: usize,
        project: &PendingProject,
    ) -> anyhow::Result<()> {
        let hit_y = row.origin.y.max(clip.origin.y);
        let hit_h = (row.origin.y + row.size.height).min(clip.origin.y + clip.size.height) - hit_y;
        if hit_h <= 0.0 {
            return Ok(());
        }
        self.widgets.push(
            rect(row.origin.x, hit_y, row.size.width, hit_h),
            WidgetKind::SidebarRow,
            OnboardingAction::ToggleProject(index),
        );
        let hovered = self.interaction.hovered == Some(OnboardingAction::ToggleProject(index));
        let bg = if project.selected {
            palette.sidebar_row_active_bg
        } else if hovered {
            palette.sidebar_row_hover_bg
        } else {
            palette.control_bg
        };
        ctx.draw_rounded_frame(
            layers,
            0,
            row.origin.x,
            row.origin.y,
            row.size.width,
            row.size.height,
            bg,
            palette.control_border,
            ctx.px(CARD_RADIUS),
        )?;
        let checkbox = rect(
            row.origin.x + ctx.px(16.0),
            row.origin.y + ctx.px(24.0),
            ctx.px(28.0),
            ctx.px(28.0),
        );
        ctx.draw_rounded_frame(
            layers,
            1,
            checkbox.origin.x,
            checkbox.origin.y,
            checkbox.size.width,
            checkbox.size.height,
            if project.selected {
                palette.selected_bg
            } else {
                palette.control_bg
            },
            if project.selected {
                palette.selected_bg
            } else {
                palette.control_border
            },
            ctx.px(6.0),
        )?;
        if project.selected {
            ctx.draw_svg_icon(
                layers,
                SvgIcon::CircleCheck,
                checkbox.origin.x + ctx.px(3.0),
                checkbox.origin.y + ctx.px(3.0),
                ctx.px(22.0),
                palette.selected_text,
            )?;
        }
        ctx.draw_text(
            layers,
            font,
            row.origin.x + ctx.px(62.0),
            row.origin.y + ctx.px(16.0),
            &project.name,
            palette.text,
            row.size.width - ctx.px(154.0),
        )?;
        let line_h = Self::line_h(ctx);
        ctx.draw_text(
            layers,
            font,
            row.origin.x + ctx.px(62.0),
            row.origin.y + ctx.px(16.0) + line_h,
            &project.path.display().to_string(),
            palette.secondary_text,
            row.size.width - ctx.px(154.0),
        )?;
        ctx.draw_text(
            layers,
            font,
            row.origin.x + row.size.width - ctx.px(132.0),
            row.origin.y + ctx.px(16.0),
            project.source.label(),
            palette.muted_text,
            ctx.px(76.0),
        )?;
        let remove = rect(
            row.origin.x + row.size.width - ctx.px(46.0),
            row.origin.y + ctx.px(22.0),
            ctx.px(32.0),
            ctx.px(32.0),
        );
        self.widgets.push(
            remove,
            WidgetKind::Button,
            OnboardingAction::RemoveProject(index),
        );
        let remove_hover = self.interaction.hovered == Some(OnboardingAction::RemoveProject(index));
        if remove_hover {
            ctx.draw_rounded_rect(
                layers,
                1,
                remove.origin.x,
                remove.origin.y,
                remove.size.width,
                remove.size.height,
                palette.control_hover_bg,
                ctx.px(8.0),
            )?;
        }
        ctx.draw_svg_icon(
            layers,
            SvgIcon::X,
            remove.origin.x + ctx.px(8.0),
            remove.origin.y + ctx.px(8.0),
            ctx.px(16.0),
            palette.muted_text,
        )
    }

    fn paint_empty_state(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        let line_h = Self::line_h(ctx);
        let card = rect(
            area.origin.x,
            area.origin.y,
            area.size.width.min(ctx.px(720.0)),
            line_h * 3.4,
        );
        ctx.draw_rounded_frame(
            layers,
            0,
            card.origin.x,
            card.origin.y,
            card.size.width,
            card.size.height,
            palette.control_bg,
            palette.control_border,
            ctx.px(CARD_RADIUS),
        )?;
        ctx.draw_svg_icon(
            layers,
            SvgIcon::FolderOpen,
            card.origin.x + ctx.px(22.0),
            card.origin.y + ctx.px(26.0),
            ctx.px(34.0),
            palette.muted_text,
        )?;
        ctx.draw_text(
            layers,
            font,
            card.origin.x + ctx.px(76.0),
            card.origin.y + ctx.px(24.0),
            "No projects queued",
            palette.text,
            card.size.width - ctx.px(98.0),
        )?;
        self.draw_wrapped_text(
            ctx,
            layers,
            font,
            card.origin.x + ctx.px(76.0),
            card.origin.y + ctx.px(24.0) + line_h,
            card.size.width - ctx.px(98.0),
            "Go back to choose a folder manually, continue with an empty workspace, or Skip setup.",
            palette.secondary_text,
            2,
        )
    }

    fn paint_preference_group(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        title: &str,
        choices: &[(&str, OnboardingAction, bool)],
    ) -> anyhow::Result<()> {
        let line_h = Self::line_h(ctx);
        ctx.draw_text(
            layers,
            section_font,
            area.origin.x,
            area.origin.y,
            title,
            palette.text,
            area.size.width,
        )?;
        let columns = 2usize;
        let gap = ctx.px(18.0);
        let control_h = Self::preference_choice_h(ctx);
        let width = ((area.size.width - gap) / columns as f32).max(ctx.px(180.0));
        let start_y = area.origin.y + line_h + ctx.px(26.0);
        for (idx, (label, action, selected)) in choices.iter().enumerate() {
            let col = idx % columns;
            let row = idx / columns;
            let x = area.origin.x + col as f32 * (width + gap);
            let y = start_y + row as f32 * (control_h + ctx.px(18.0));
            self.paint_choice_pill(
                ctx,
                layers,
                rect(x, y, width, control_h),
                palette,
                font,
                label,
                *selected,
                *action,
            )?;
        }
        Ok(())
    }

    fn paint_choice_pill(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        label: &str,
        selected: bool,
        action: OnboardingAction,
    ) -> anyhow::Result<()> {
        self.widgets.push(area, WidgetKind::Button, action);
        let hovered = self.interaction.hovered == Some(action);
        let pressed = self.interaction.pressed == Some(action);
        let bg = if selected {
            palette.selected_bg.mul_alpha(0.22)
        } else if pressed {
            palette.control_pressed_bg
        } else if hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if selected {
            palette.selected_bg
        } else {
            palette.control_border
        };
        ctx.draw_rounded_frame(
            layers,
            0,
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            bg,
            border,
            ctx.px(CARD_RADIUS),
        )?;
        ctx.draw_text(
            layers,
            font,
            area.origin.x + ctx.px(22.0),
            Self::centered_text_y(ctx, area),
            label,
            if selected {
                palette.text
            } else {
                palette.secondary_text
            },
            area.size.width - ctx.px(44.0),
        )
    }

    fn paint_button(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        label: &str,
        action: OnboardingAction,
        primary: bool,
        enabled: bool,
    ) -> anyhow::Result<()> {
        self.widgets.push(area, WidgetKind::Button, action);
        let state = if !enabled {
            ControlState::Disabled
        } else if self.interaction.pressed == Some(action) {
            ControlState::Pressed
        } else if self.interaction.hovered == Some(action) {
            ControlState::Hovered
        } else {
            ControlState::Normal
        };
        let (mut bg, mut border) = state.colors(palette);
        let mut text = palette.text;
        if primary && enabled {
            bg = if state == ControlState::Pressed {
                palette.selected_bg.mul_alpha(0.78)
            } else {
                palette.selected_bg
            };
            border = palette.selected_bg;
            text = palette.selected_text;
        } else if !enabled {
            bg = bg.mul_alpha(0.50);
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
            ctx.px(CARD_RADIUS),
        )?;
        let text_w = ctx.measure_text_width(font, label);
        let max_text_w = (area.size.width - 24.0).max(0.0);
        let x =
            area.origin.x + ((area.size.width - text_w.min(max_text_w)) / 2.0).max(ctx.px(12.0));
        ctx.draw_text(
            layers,
            font,
            x,
            Self::centered_text_y(ctx, area),
            label,
            text,
            max_text_w,
        )
    }

    fn paint_summary_row(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        row: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        label: &str,
        value: &str,
    ) -> anyhow::Result<()> {
        ctx.draw_rounded_frame(
            layers,
            0,
            row.origin.x,
            row.origin.y,
            row.size.width,
            row.size.height,
            palette.control_bg,
            palette.control_border,
            ctx.px(CARD_RADIUS),
        )?;
        ctx.draw_text(
            layers,
            font,
            row.origin.x + ctx.px(18.0),
            Self::centered_text_y(ctx, row),
            label,
            palette.secondary_text,
            row.size.width * 0.42,
        )?;
        ctx.draw_text(
            layers,
            font,
            row.origin.x + row.size.width * 0.45,
            Self::centered_text_y(ctx, row),
            value,
            palette.text,
            row.size.width * 0.52,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_wrapped_text(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        x: f32,
        mut y: f32,
        width: f32,
        text: &str,
        color: LinearRgba,
        max_lines: usize,
    ) -> anyhow::Result<()> {
        if max_lines == 0 || width <= 0.0 || text.is_empty() {
            return Ok(());
        }
        let mut lines = Vec::new();
        let mut current = String::new();
        let mut consumed_words = 0usize;
        let words = text.split_whitespace().collect::<Vec<_>>();
        for word in &words {
            let candidate = if current.is_empty() {
                (*word).to_string()
            } else {
                format!("{current} {word}")
            };
            if current.is_empty() || ctx.measure_text_width(font, &candidate) <= width {
                current = candidate;
                consumed_words += 1;
            } else {
                lines.push(current);
                current = (*word).to_string();
                consumed_words += 1;
                if lines.len() == max_lines {
                    break;
                }
            }
        }
        if !current.is_empty() && lines.len() < max_lines {
            lines.push(current);
        }
        let truncated = consumed_words < words.len();
        if truncated {
            if let Some(last) = lines.last_mut() {
                *last = ctx.text_with_ellipsis(font, last, width);
            }
        }
        let line_h = Self::compact_line_h(ctx);
        for line in lines.into_iter().take(max_lines) {
            ctx.draw_text(layers, font, x, y, &line, color, width)?;
            y += line_h;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct OnboardingPrefs {
    space_name: String,
    target_space_id: Option<String>,
    imported_project_count: usize,
    language: NativeLanguagePreference,
    appearance: NativeThemeMode,
    show_left_sidebar: bool,
}

fn finish_onboarding(tw: &mut TermWindow, prefs: OnboardingPrefs) {
    let mut settings = crate::native_settings::load();
    apply_onboarding_preferences(&mut settings, &prefs);
    if let Err(err) = crate::native_settings::save(&settings) {
        log::error!("failed to save onboarding settings: {err:#}");
    }
    crate::native_settings::apply_to_app(&settings);
    if let Some(front_end) = crate::frontend::try_front_end() {
        front_end.invalidate_all_windows();
    }

    let space_id = prefs
        .target_space_id
        .clone()
        .unwrap_or_else(|| crate::workspace_threads::ensure_space_named(&prefs.space_name));

    if let Some(window) = tw.window.as_ref().cloned() {
        if tw.active_space_id != space_id {
            tw.switch_space(space_id, &window);
        } else {
            window.invalidate();
        }
        if tw.workspace_sidebar_collapsed == prefs.show_left_sidebar {
            tw.workspace_sidebar_collapsed = !prefs.show_left_sidebar;
            let dimensions = tw.dimensions;
            tw.apply_dimensions(&dimensions, None, &window);
        }
    }
    log::debug!(
        "onboarding completed: imported_project_count={}",
        prefs.imported_project_count
    );
    tw.close_content_view();
}

fn apply_onboarding_preferences(settings: &mut ThinkTermNativeSettings, prefs: &OnboardingPrefs) {
    settings.appearance.theme_mode = prefs.appearance;
    settings.onboarding.language = prefs.language;
    settings.onboarding.show_left_sidebar_by_default = prefs.show_left_sidebar;
    mark_onboarding_seen(settings);
}

fn project_name_for_path(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Project")
        .to_string()
}

fn inset_rect(rect: RectF, amount: f32) -> RectF {
    euclid::rect(
        rect.origin.x + amount,
        rect.origin.y + amount,
        (rect.size.width - amount * 2.0).max(0.0),
        (rect.size.height - amount * 2.0).max(0.0),
    )
}

impl ContentView for OnboardingView {
    fn title(&self) -> String {
        "ThinkTerm Setup".to_string()
    }

    fn tab_key(&self) -> Option<String> {
        Some(format!("onboarding:{}", self.initial_space_id))
    }

    fn space_id(&self) -> Option<&str> {
        Some(&self.initial_space_id)
    }

    fn wants_cursor_blink(&self) -> bool {
        false
    }

    fn next_frame_time(&self) -> Option<Instant> {
        self.transition
            .filter(|transition| transition.started_at.elapsed() < Self::transition_duration())
            .map(|_| Instant::now() + Duration::from_millis(16))
    }

    fn paint(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        cursor_on: bool,
    ) -> anyhow::Result<()> {
        self.paint_impl(
            ctx,
            layers,
            area,
            palette,
            font,
            title_font,
            section_font,
            cursor_on,
        )
    }

    fn on_mouse(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse {
        self.on_mouse_impl(x, y, kind)
    }

    fn on_key(&mut self, key: KeyCode, mods: KeyModifiers) -> ContentViewResponse {
        self.on_key_impl(key, mods)
    }

    fn on_paste(&mut self, text: &str) -> ContentViewResponse {
        self.on_paste_impl(text)
    }

    fn on_close_requested(&mut self) -> ContentViewResponse {
        self.skip_response()
    }

    fn on_folder_picked(&mut self, path: PathBuf) -> ContentViewResponse {
        self.add_manual_project(path);
        ContentViewResponse::Redraw
    }

    fn copy_text(&self) -> Option<String> {
        None
    }

    fn cut_text(&mut self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_project_uses_folder_name_and_starts_selected() {
        let project = PendingProject::from_path(
            PathBuf::from("/tmp/thinkterm-onboarding-demo"),
            ProjectSource::Manual,
        );

        assert_eq!(project.name, "thinkterm-onboarding-demo");
        assert!(project.selected);
        assert_eq!(project.source.label(), "Manual");
    }

    #[test]
    fn selected_project_count_tracks_toggles() {
        let mut view = OnboardingView::new("space-default".to_string(), "Default".to_string());
        view.pending_projects.push(PendingProject::from_path(
            PathBuf::from("/tmp/one"),
            ProjectSource::Manual,
        ));
        view.pending_projects.push(PendingProject::from_path(
            PathBuf::from("/tmp/two"),
            ProjectSource::Manual,
        ));

        assert_eq!(view.selected_project_count(), 2);
        view.pending_projects[1].selected = false;
        assert_eq!(view.selected_project_count(), 1);
    }

    #[test]
    fn flow_skips_space_step() {
        assert_eq!(Step::all().len(), 5);
        assert_eq!(Step::Welcome.next(), Step::ImportSources);
        assert_eq!(Step::ImportSources.previous(), Step::Welcome);
    }

    #[test]
    fn target_space_uses_initial_space() {
        let view = OnboardingView::new("space-default".to_string(), "Default".to_string());
        assert_eq!(view.normalized_space_name(), "Default");
        assert_eq!(view.target_space_id_for_choice(), "space-default");
    }

    #[test]
    fn unavailable_providers_are_not_reported_available() {
        assert!(ImportProvider::Manual.is_available());
        assert!(!ImportProvider::VsCode.is_available());
        assert!(!ImportProvider::Cursor.is_available());
        assert!(!ImportProvider::Antigravity.is_available());
        assert!(!ImportProvider::Cmux.is_available());
    }
}
