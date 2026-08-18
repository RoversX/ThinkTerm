//! First-run setup: one page, no wizard.
//!
//! It asks two things — what language, and light or dark — and gets out of the
//! way.
//!
//! Every surface here is a `draw_rounded_frame` and every glyph a `SvgIcon`,
//! which is the combination the rest of this crate already draws with. An
//! earlier cut invented its own ornament — a bevelled slab with a phosphor-dot
//! grid for the mark — by transcribing a CSS mock. Those primitives have no
//! equivalent here (no blend modes, no clipping, no box-shadow) and the result
//! did not render as designed. Prefer a plainer thing that is certainly right.
//!
//! The palette is deliberately neutral. The user is choosing a theme on this
//! screen, so an accent hue would compete with the very thing being judged;
//! selection is carried by fill and border weight instead.

use crate::i18n::{language_option_label, LANGUAGE_OPTIONS};
use crate::native_settings::{mark_onboarding_seen, NativeThemeMode, ThinkTermNativeSettings};
use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::content_view::{ContentView, ContentViewResponse};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::TermWindow;
use crate::ui::{
    rect, ControlState, DrawContext, InteractionState, UiContext, UiPalette, WidgetKind,
};
use crate::utilsprites::RenderMetrics;
use std::rc::Rc;
use std::time::Instant;
use wezterm_font::LoadedFont;
use wezterm_term::{KeyCode, KeyModifiers};
use window::color::LinearRgba;
use window::{MouseEventKind as WMEK, MousePress, RectF, WindowOps};

// Design pixels, which `ctx.px()` maps onto this surface's backing grid.
//
// A design pixel is *half* a CSS/logical pixel: the design dpi is 144, i.e. a
// 2x macOS surface (see `ui::tokens::ui_scale_for_dpi`). So a control that
// should look 32pt tall is 64 here. Getting this wrong halves the whole layout
// while leaving the text at full size, which reads as everything crushed
// together and wrapping early.
const COL_W: f32 = 1120.0;
/// Floor for the content column, so text never gets a zero width budget.
const MIN_COL_W: f32 = 320.0;
const SIDE_PAD: f32 = 64.0;
const SECTION_GAP: f32 = 60.0;

const MARK_SIZE: f32 = 128.0;
const MARK_TO_TITLE: f32 = 52.0;
const TITLE_TO_SUB: f32 = 12.0;
const LABEL_GAP: f32 = 24.0;

const CHIP_PAD_X: f32 = 28.0;
const CHIP_PAD_Y: f32 = 18.0;
const CHIP_GAP: f32 = 16.0;
const CHIP_RADIUS: f32 = 18.0;

/// Appearance previews. Big enough to actually depict a light and a dark
/// surface, because that is the one thing on this page that communicates
/// without being read — the user may not have picked their language yet.
const TILE_W: f32 = 224.0;
const TILE_H: f32 = 144.0;
const TILE_GAP: f32 = 24.0;
const TILE_RADIUS: f32 = 20.0;
const TILE_INSET: f32 = 16.0;
const TILE_LABEL_GAP: f32 = 16.0;
/// Slack around a preview's caption, so a long translation widens the tile
/// instead of being ellipsised inside it.
const TILE_CAPTION_PAD: f32 = 24.0;
const FACE_RADIUS: f32 = 10.0;
const FACE_SPLIT_GAP: f32 = 8.0;
const BAR_H: f32 = 6.0;
const BAR_GAP: f32 = 10.0;
const BAR_INSET: f32 = 12.0;

/// Ring weights. `draw_rounded_frame`'s own border is always exactly one
/// physical pixel, so anything heavier has to be a filled rounded rect drawn
/// *behind* the control, with the margin showing as the ring.
/// Floor for the shrink factor; below this the page is unreadable regardless.
const FIT_MIN: f32 = 0.42;

const RING_SELECTED: f32 = 4.0;
const RING_FOCUS: f32 = 6.0;

const BTN_PAD_X: f32 = 40.0;
const BTN_PAD_Y: f32 = 22.0;
const BTN_GAP: f32 = 20.0;
const BTN_RADIUS: f32 = 18.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum OnboardingAction {
    Start,
    Skip,
    /// Carries the stable preference string from [`LANGUAGE_OPTIONS`], which is
    /// what `localization.language` persists, rather than the legacy
    /// `NativeLanguagePreference` enum that cannot name every shipped locale.
    Language(&'static str),
    Appearance(NativeThemeMode),
}

/// How far the discretionary space must shrink for the page to fit.
///
/// Text height is fixed, so total height is `rigid + factor * flex` — linear,
/// and solvable in one step. Floored rather than allowed to go to zero: past
/// some point the page is unreadable anyway, and overflowing slightly beats
/// collapsing every gap to nothing.
fn fit_factor(available: f32, rigid: f32, flex: f32) -> f32 {
    if flex <= 0.0 {
        return 1.0;
    }
    ((available - rigid) / flex).clamp(FIT_MIN, 1.0)
}

/// The appearance choices with their labels already localized.
///
/// Extracted so a test can assert the labels really are translations: an
/// earlier cut left a second `modes` binding holding the raw key strings, which
/// shadowed the translated one at the draw site, and the previews rendered
/// "onboarding-theme-…" truncated to fit.
fn appearance_choices() -> Vec<(NativeThemeMode, String)> {
    vec![
        (
            NativeThemeMode::System,
            crate::i18n::tr("onboarding-theme-system"),
        ),
        (
            NativeThemeMode::Light,
            crate::i18n::tr("onboarding-theme-light"),
        ),
        (
            NativeThemeMode::Dark,
            crate::i18n::tr("onboarding-theme-dark"),
        ),
    ]
}

/// Tab order. Keyboard users get the same reach as the mouse, which the
/// multi-step version never offered — there, Tab switched steps and no control
/// was reachable without pointing at it.
fn focus_order() -> Vec<OnboardingAction> {
    let mut order: Vec<OnboardingAction> = LANGUAGE_OPTIONS
        .iter()
        .map(|option| OnboardingAction::Language(option.preference))
        .collect();
    order.extend(
        [
            NativeThemeMode::System,
            NativeThemeMode::Light,
            NativeThemeMode::Dark,
        ]
        .map(OnboardingAction::Appearance),
    );
    order.push(OnboardingAction::Skip);
    order.push(OnboardingAction::Start);
    order
}

pub(crate) struct OnboardingView {
    widgets: UiContext<OnboardingAction>,
    interaction: InteractionState<OnboardingAction>,
    initial_space_id: String,
    initial_space_name: String,
    /// Preference string from [`LANGUAGE_OPTIONS`], not the legacy enum.
    selected_language: &'static str,
    selected_appearance: NativeThemeMode,
    /// What the app was using when this opened. Choices apply the instant they
    /// are clicked, so without these Skip would have nothing to undo and would
    /// be indistinguishable from Get Started — while still promising otherwise.
    opened_with: (&'static str, NativeThemeMode),
}

impl OnboardingView {
    pub(crate) fn new(initial_space_id: String, initial_space_name: String) -> Self {
        let settings = crate::native_settings::load();
        Self {
            widgets: UiContext::default(),
            interaction: InteractionState::default(),
            initial_space_id,
            initial_space_name,
            // Seeded from the effective preference, which already prefers
            // `localization.language` and falls back to the legacy field, so a
            // language set in Settings shows up preselected here.
            selected_language: language_preference_for(&settings),
            selected_appearance: settings.appearance.theme_mode,
            opened_with: (
                language_preference_for(&settings),
                settings.appearance.theme_mode,
            ),
        }
    }

    fn space_name(&self) -> String {
        let initial_name = self.initial_space_name.trim();
        if !initial_name.is_empty() {
            return initial_name.to_string();
        }
        let initial_id = self.initial_space_id.trim();
        if initial_id.is_empty() {
            "Default".to_string()
        } else {
            initial_id.to_string()
        }
    }

    fn target_space_id_for_choice(&self) -> String {
        let initial_id = self.initial_space_id.trim();
        if initial_id.is_empty() {
            crate::workspace_threads::ensure_space_named(&self.space_name())
        } else {
            self.initial_space_id.clone()
        }
    }

    // ---------------------------------------------------------------- input

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
                // Clicking also takes focus, so Tab continues from where the
                // pointer left off rather than jumping back to the start.
                if hit.is_some() {
                    self.interaction.focused = hit;
                }
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

    /// Both choices apply the moment they are picked, exactly as the Settings
    /// window applies them — the page redraws in the new language and the whole
    /// app switches theme. Deferring them to "Get Started" made the controls
    /// look inert: you click 简体中文 and nothing happens.
    fn apply(&mut self, action: OnboardingAction) -> ContentViewResponse {
        match action {
            OnboardingAction::Start => self.finish_response(),
            OnboardingAction::Skip => self.skip_response(),
            OnboardingAction::Language(language) => {
                // Redraw, not Ignored: the press fill is derived from
                // `state_of`, so skipping the repaint leaves the control stuck
                // looking pressed after the button comes back up.
                if self.selected_language == language {
                    return ContentViewResponse::Redraw;
                }
                self.selected_language = language;
                ContentViewResponse::Run(Box::new(move |_tw: &mut TermWindow| {
                    let mut settings = crate::native_settings::load();
                    settings.localization.language = Some(language.to_string());
                    if let Err(err) = crate::native_settings::save(&settings) {
                        log::error!("failed to save onboarding language: {err:#}");
                        return;
                    }
                    crate::i18n::activate_preference(language);
                    if let Some(front_end) = crate::frontend::try_front_end() {
                        front_end.invalidate_all_windows();
                    }
                }))
            }
            OnboardingAction::Appearance(mode) => {
                if self.selected_appearance == mode {
                    return ContentViewResponse::Redraw;
                }
                self.selected_appearance = mode;
                ContentViewResponse::Run(Box::new(move |_tw: &mut TermWindow| {
                    let mut settings = crate::native_settings::load();
                    settings.appearance.theme_mode = mode;
                    if let Err(err) = crate::native_settings::save(&settings) {
                        log::error!("failed to save onboarding appearance: {err:#}");
                        return;
                    }
                    crate::native_settings::apply_to_app(&settings);
                    if let Some(front_end) = crate::frontend::try_front_end() {
                        front_end.invalidate_all_windows();
                    }
                }))
            }
        }
    }

    fn move_focus(&mut self, forward: bool) -> ContentViewResponse {
        let order = focus_order();
        if order.is_empty() {
            return ContentViewResponse::Ignored;
        }
        let next = match self
            .interaction
            .focused
            .and_then(|current| order.iter().position(|item| *item == current))
        {
            Some(index) if forward => (index + 1) % order.len(),
            Some(index) => (index + order.len() - 1) % order.len(),
            // No focus yet: Tab enters at the top, Shift+Tab at the bottom.
            None if forward => 0,
            None => order.len() - 1,
        };
        self.interaction.focused = Some(order[next]);
        ContentViewResponse::Redraw
    }

    fn on_key_impl(&mut self, key: KeyCode, mods: KeyModifiers) -> ContentViewResponse {
        match (key, mods) {
            (KeyCode::Escape, _) => self.skip_response(),
            (KeyCode::Tab, KeyModifiers::NONE) => self.move_focus(true),
            (KeyCode::Tab, KeyModifiers::SHIFT) => self.move_focus(false),
            // Space activates whatever is focused. Enter is the default
            // action — it starts — except on Skip, which it honours. Focus on a
            // chip does not change that: the chip is already applied the moment
            // it is picked, so there is nothing left for Enter to confirm.
            (KeyCode::Char(' '), _) => match self.interaction.focused {
                Some(action) => self.apply(action),
                None => ContentViewResponse::Ignored,
            },
            (KeyCode::Enter, _) => match self.interaction.focused {
                Some(OnboardingAction::Skip) => self.skip_response(),
                _ => self.finish_response(),
            },
            _ => ContentViewResponse::Ignored,
        }
    }

    fn finish_response(&self) -> ContentViewResponse {
        let prefs = OnboardingPrefs {
            space_name: self.space_name(),
            target_space_id: Some(self.target_space_id_for_choice()),
            language: self.selected_language,
            appearance: self.selected_appearance,
        };
        ContentViewResponse::Run(Box::new(move |tw: &mut TermWindow| {
            finish_onboarding(tw, prefs);
        }))
    }

    /// Skip puts back whatever the app was using when this opened, then marks
    /// setup seen. Anything picked here has already been applied, so without
    /// the restore "Skip" would silently keep the very choices it offers to
    /// discard.
    fn skip_response(&self) -> ContentViewResponse {
        let (language, appearance) = self.opened_with;
        let changed = (self.selected_language, self.selected_appearance) != self.opened_with;
        ContentViewResponse::Run(Box::new(move |tw: &mut TermWindow| {
            let mut settings = crate::native_settings::load();
            if changed {
                settings.localization.language = Some(language.to_string());
                settings.appearance.theme_mode = appearance;
            }
            mark_onboarding_seen(&mut settings);
            if let Err(err) = crate::native_settings::save(&settings) {
                log::error!("failed to save onboarding skip state: {err:#}");
            }
            if changed {
                crate::native_settings::apply_to_app(&settings);
                crate::i18n::activate_preference(language);
                if let Some(front_end) = crate::frontend::try_front_end() {
                    front_end.invalidate_all_windows();
                }
            }
            tw.close_content_view();
        }))
    }

    // ---------------------------------------------------------------- paint

    fn state_of(&self, action: OnboardingAction) -> ControlState {
        if self.interaction.pressed == Some(action) {
            ControlState::Pressed
        } else if self.interaction.hovered == Some(action) {
            ControlState::Hovered
        } else {
            ControlState::Normal
        }
    }

    fn text_h(font: &Rc<LoadedFont>) -> f32 {
        RenderMetrics::with_font_metrics(&font.metrics())
            .cell_size
            .height as f32
    }

    /// One chip's width: its label plus symmetric padding.
    fn chip_w(ctx: &DrawContext, font: &Rc<LoadedFont>, label: &str) -> f32 {
        ctx.measure_text_width(font, label) + ctx.px(CHIP_PAD_X) * 2.0
    }

    /// Wrap chips into rows that fit `max_w`. Returns one vector of indices per
    /// row. Five languages fit on one row at 560px, but a narrow window (or a
    /// locale with longer names) has to wrap rather than overflow.
    fn chip_rows(widths: &[f32], gap: f32, max_w: f32) -> Vec<Vec<usize>> {
        let mut rows: Vec<Vec<usize>> = Vec::new();
        let mut row: Vec<usize> = Vec::new();
        let mut used = 0.0f32;
        for (index, width) in widths.iter().enumerate() {
            let advance = if row.is_empty() { *width } else { gap + *width };
            if !row.is_empty() && used + advance > max_w {
                rows.push(std::mem::take(&mut row));
                used = *width;
            } else {
                used += advance;
            }
            row.push(index);
        }
        if !row.is_empty() {
            rows.push(row);
        }
        rows
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
    ) -> anyhow::Result<()> {
        self.widgets.clear();
        let skin = Skin::new(palette);

        ctx.draw_rect(
            layers,
            0,
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            palette.window_bg,
        )?;

        let body_h = Self::text_h(font);
        let title_h = Self::text_h(title_font);
        let label_h = Self::text_h(section_font);

        // Floored, not clamped to zero: `draw_text` early-returns on a
        // non-positive max width, so a window narrower than the side padding
        // would drop every label while the chips and previews kept painting.
        // Better to overflow the edge than to render a page of blank shapes.
        let col_w = ctx
            .px(COL_W)
            .min((area.size.width - ctx.px(SIDE_PAD) * 2.0).max(ctx.px(MIN_COL_W)));
        let col_x = area.origin.x + ((area.size.width - col_w) / 2.0).max(0.0);

        // --- measure everything first so the column can be centred vertically
        let languages: Vec<(String, &'static str)> = LANGUAGE_OPTIONS
            .iter()
            .map(|option| (language_option_label(*option), option.preference))
            .collect();
        let chip_widths: Vec<f32> = languages
            .iter()
            .map(|(label, _)| Self::chip_w(ctx, font, label))
            .collect();
        // Wrapping is settled at full scale: shrinking only ever buys room, so
        // this is the conservative row count.
        let rows = Self::chip_rows(&chip_widths, ctx.px(CHIP_GAP), col_w);

        let modes = appearance_choices();
        // A preview is at least as wide as the widest caption. At the design
        // width the French "Suivre le système" was ellipsised to "Suivre le
        // syst…", which is the one label a user who cannot yet read the UI
        // most needs whole.
        let widest_caption = modes
            .iter()
            .map(|(_, label)| ctx.measure_text_width(font, label))
            .fold(0.0f32, f32::max);
        let tile_w_base = ctx
            .px(TILE_W)
            .max(widest_caption + ctx.px(TILE_CAPTION_PAD));
        let tile_row_w = tile_w_base * modes.len() as f32
            + ctx.px(TILE_GAP) * (modes.len().saturating_sub(1)) as f32;
        let tiles_fit = tile_row_w <= col_w;

        // Text cannot shrink; spacing and decoration can. Page height is
        // therefore linear in a single factor, so solve it rather than guess:
        // at the default 24-row window the content area is only ~700px tall and
        // the full-size layout runs ~340px past it — with no scrolling and no
        // wheel handler, that put "Get Started" off screen and out of reach.
        let rows_n = rows.len() as f32;
        let tile_rows = if tiles_fit { 1.0 } else { modes.len() as f32 };
        let rigid = title_h
            + body_h
            + label_h
            + rows_n * body_h
            + label_h
            + tile_rows * body_h
            + body_h
            + body_h;
        let flex = ctx.px(MARK_SIZE
            + MARK_TO_TITLE
            + TITLE_TO_SUB
            + LABEL_GAP * 2.0
            + rows_n * CHIP_PAD_Y * 2.0
            + (rows_n - 1.0).max(0.0) * CHIP_GAP
            + tile_rows * (TILE_H + TILE_LABEL_GAP)
            + (tile_rows - 1.0).max(0.0) * TILE_GAP
            + BTN_PAD_Y * 2.0
            + SECTION_GAP * 4.0);
        let fit = fit_factor(area.size.height - ctx.px(SIDE_PAD) * 2.0, rigid, flex);
        // Every discretionary dimension goes through this from here on.
        let fx = |value: f32| ctx.px(value) * fit;

        let chip_h = body_h + fx(CHIP_PAD_Y) * 2.0;
        let chips_h = rows_n * chip_h + (rows_n - 1.0).max(0.0) * fx(CHIP_GAP);
        let tile_w = tile_w_base * fit;
        let tile_h = fx(TILE_H);
        let modes_h = tile_rows * (tile_h + fx(TILE_LABEL_GAP) + body_h)
            + (tile_rows - 1.0).max(0.0) * fx(TILE_GAP);
        let brand_h = fx(MARK_SIZE) + fx(MARK_TO_TITLE) + title_h + fx(TITLE_TO_SUB) + body_h;
        let group_h = |controls: f32| label_h + fx(LABEL_GAP) + controls;
        let actions_h = body_h + fx(BTN_PAD_Y) * 2.0;

        let gap = fx(SECTION_GAP);
        let total_h = brand_h
            + gap
            + group_h(chips_h)
            + gap
            + group_h(modes_h)
            + gap
            + body_h
            + gap
            + actions_h;

        let mut y = area.origin.y + ((area.size.height - total_h) / 2.0).max(0.0);

        // --- brand
        self.paint_mark(
            ctx,
            layers,
            rect(
                col_x + (col_w - fx(MARK_SIZE)) / 2.0,
                y,
                fx(MARK_SIZE),
                fx(MARK_SIZE),
            ),
            skin,
        )?;
        y += fx(MARK_SIZE) + fx(MARK_TO_TITLE);

        let title = crate::i18n::tr("onboarding-title");
        Self::draw_centered(
            ctx,
            layers,
            title_font,
            col_x,
            y,
            col_w,
            &title,
            palette.text,
        )?;
        y += title_h + fx(TITLE_TO_SUB);

        let subtitle = crate::i18n::tr("onboarding-subtitle");
        Self::draw_centered(
            ctx,
            layers,
            font,
            col_x,
            y,
            col_w,
            &subtitle,
            palette.muted_text,
        )?;
        y += body_h + gap;

        // --- language
        ctx.draw_text(
            layers,
            section_font,
            col_x,
            y,
            &crate::i18n::tr("onboarding-language"),
            skin.label,
            col_w,
        )?;
        y += label_h + fx(LABEL_GAP);

        for row in &rows {
            let mut x = col_x;
            for index in row {
                let (label, preference) = &languages[*index];
                let action = OnboardingAction::Language(preference);
                self.paint_chip(
                    ctx,
                    layers,
                    rect(x, y, chip_widths[*index], chip_h),
                    skin,
                    font,
                    label,
                    action,
                    self.selected_language == *preference,
                )?;
                x += chip_widths[*index] + fx(CHIP_GAP);
            }
            y += chip_h + fx(CHIP_GAP);
        }
        y -= fx(CHIP_GAP);
        y += gap;

        // --- appearance
        ctx.draw_text(
            layers,
            section_font,
            col_x,
            y,
            &crate::i18n::tr("onboarding-appearance"),
            skin.label,
            col_w,
        )?;
        y += label_h + fx(LABEL_GAP);

        let tile_step = tile_h + fx(TILE_LABEL_GAP) + body_h + fx(TILE_GAP);
        for (index, (mode, label)) in modes.iter().enumerate() {
            let (tx, ty) = if tiles_fit {
                (col_x + index as f32 * (tile_w + fx(TILE_GAP)), y)
            } else {
                (col_x, y + index as f32 * tile_step)
            };
            self.paint_theme_tile(
                ctx,
                layers,
                rect(tx, ty, tile_w, tile_h),
                skin,
                font,
                *mode,
                label,
                fit,
            )?;
        }
        y += modes_h + gap;

        // --- privacy footnote
        ctx.draw_text(
            layers,
            font,
            col_x,
            y,
            &crate::i18n::tr("onboarding-privacy"),
            palette.muted_text,
            col_w,
        )?;
        y += body_h + gap;

        // --- actions, right aligned
        let start_label = crate::i18n::tr("onboarding-start");
        let skip_label = crate::i18n::tr("onboarding-skip");
        let start_w = ctx.measure_text_width(font, &start_label) + ctx.px(BTN_PAD_X) * 2.0;
        let skip_w = ctx.measure_text_width(font, &skip_label) + ctx.px(BTN_PAD_X) * 2.0;

        let start_x = col_x + col_w - start_w;
        self.paint_button(
            ctx,
            layers,
            rect(start_x, y, start_w, actions_h),
            skin,
            font,
            &start_label,
            OnboardingAction::Start,
            true,
        )?;
        self.paint_button(
            ctx,
            layers,
            rect(start_x - ctx.px(BTN_GAP) - skip_w, y, skip_w, actions_h),
            skin,
            font,
            &skip_label,
            OnboardingAction::Skip,
            false,
        )?;

        Ok(())
    }

    /// The ThinkTerm mark: a disc carrying the prompt chevron, and nothing
    /// else. The app icon's gloss and phosphor grid are deliberately not
    /// reproduced; there is no primitive here that gets them right, and an
    /// approximation of them looks like a defect.
    fn paint_mark(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        card: RectF,
        skin: Skin,
    ) -> anyhow::Result<()> {
        // A radius of half the side is clamped to exactly that, giving a
        // circle — the mark is square, so this is a disc rather than a stadium.
        ctx.draw_rounded_frame(
            layers,
            0,
            card.origin.x,
            card.origin.y,
            card.size.width,
            card.size.height,
            skin.mark_bg,
            skin.mark_border,
            card.size.width / 2.0,
        )?;
        let glyph = card.size.width * 0.5;
        ctx.draw_svg_icon(
            layers,
            SvgIcon::ChevronRight,
            card.origin.x + (card.size.width - glyph) / 2.0,
            card.origin.y + (card.size.height - glyph) / 2.0,
            glyph,
            skin.text,
        )
    }

    /// An appearance preview: a framed card holding one light face, one dark
    /// face, or both side by side. Each face is its own rounded rect, so the
    /// split needs no clipping — which this renderer does not have.
    #[allow(clippy::too_many_arguments)]
    fn paint_theme_tile(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        tile: RectF,
        skin: Skin,
        font: &Rc<LoadedFont>,
        mode: NativeThemeMode,
        label: &str,
        fit: f32,
    ) -> anyhow::Result<()> {
        // The tile box is scaled by the page's fit factor, so everything drawn
        // inside it has to scale too — otherwise a short window shrinks the
        // frame while its insets and text bars stay full size and spill out.
        let fx = |value: f32| ctx.px(value) * fit;
        let caption_h = fx(TILE_LABEL_GAP) + Self::text_h(font);
        let action = OnboardingAction::Appearance(mode);
        let selected = self.selected_appearance == mode;
        // The hit target covers the caption too. It is drawn below the card and
        // is the only text naming each theme — the obvious thing to click,
        // especially before the language is chosen.
        self.widgets.push(
            rect(
                tile.origin.x,
                tile.origin.y,
                tile.size.width,
                tile.size.height + caption_h,
            ),
            WidgetKind::Button,
            action,
        );

        // Rings first: they are filled rounded rects showing through as a
        // margin, so they have to be under the card.
        self.paint_ring(ctx, layers, tile, skin, action, fx(TILE_RADIUS), selected)?;

        let card_bg = match self.state_of(action) {
            ControlState::Pressed => skin.chip_pressed_bg,
            ControlState::Hovered => skin.chip_hover_bg,
            _ => skin.chip_bg,
        };
        ctx.draw_rounded_frame(
            layers,
            0,
            tile.origin.x,
            tile.origin.y,
            tile.size.width,
            tile.size.height,
            card_bg,
            if selected {
                skin.chip_selected_border
            } else {
                skin.chip_border
            },
            fx(TILE_RADIUS),
        )?;

        let inset = fx(TILE_INSET);
        let inner = rect(
            tile.origin.x + inset,
            tile.origin.y + inset,
            (tile.size.width - inset * 2.0).max(0.0),
            (tile.size.height - inset * 2.0).max(0.0),
        );
        match mode {
            NativeThemeMode::System => {
                let gap = fx(FACE_SPLIT_GAP);
                let half = ((inner.size.width - gap) / 2.0).max(0.0);
                self.paint_face(
                    ctx,
                    layers,
                    rect(inner.origin.x, inner.origin.y, half, inner.size.height),
                    skin,
                    true,
                    fit,
                )?;
                self.paint_face(
                    ctx,
                    layers,
                    rect(
                        inner.origin.x + half + gap,
                        inner.origin.y,
                        half,
                        inner.size.height,
                    ),
                    skin,
                    false,
                    fit,
                )?;
            }
            NativeThemeMode::Light => self.paint_face(ctx, layers, inner, skin, true, fit)?,
            NativeThemeMode::Dark => self.paint_face(ctx, layers, inner, skin, false, fit)?,
        }

        let text_w = ctx.measure_text_width(font, label).min(tile.size.width);
        ctx.draw_text(
            layers,
            font,
            tile.origin.x + ((tile.size.width - text_w) / 2.0).max(0.0),
            tile.origin.y + tile.size.height + fx(TILE_LABEL_GAP),
            label,
            if selected {
                skin.text
            } else {
                skin.secondary_text
            },
            tile.size.width,
        )
    }

    /// One face of a preview: a light or dark surface with a couple of bars
    /// standing in for text. Fixed colours — it depicts a theme, so it must not
    /// follow the current one.
    fn paint_face(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        face: RectF,
        skin: Skin,
        light: bool,
        fit: f32,
    ) -> anyhow::Result<()> {
        let fx = |value: f32| ctx.px(value) * fit;
        let (bg, bar, bar_hi) = if light {
            (
                LinearRgba::with_srgba(0xF2, 0xF2, 0xF6, 255),
                LinearRgba::with_srgba(0xC2, 0xC3, 0xCE, 255),
                LinearRgba::with_srgba(0x7C, 0x7E, 0x92, 255),
            )
        } else {
            (
                LinearRgba::with_srgba(0x1B, 0x1B, 0x21, 255),
                LinearRgba::with_srgba(0x45, 0x47, 0x59, 255),
                LinearRgba::with_srgba(0x8E, 0x90, 0xA6, 255),
            )
        };
        ctx.draw_rounded_frame(
            layers,
            0,
            face.origin.x,
            face.origin.y,
            face.size.width,
            face.size.height,
            bg,
            skin.face_border,
            fx(FACE_RADIUS),
        )?;

        let bar_h = fx(BAR_H);
        let inset = fx(BAR_INSET);
        let usable = (face.size.width - inset * 2.0).max(0.0);
        let mut y = face.origin.y + inset;
        for (index, ratio) in [0.52f32, 0.78, 0.64].iter().enumerate() {
            if y + bar_h > face.origin.y + face.size.height - inset {
                break;
            }
            ctx.draw_rounded_rect(
                layers,
                0,
                face.origin.x + inset,
                y,
                usable * ratio,
                bar_h,
                if index == 0 { bar_hi } else { bar },
                bar_h / 2.0,
            )?;
            y += bar_h + fx(BAR_GAP);
        }
        Ok(())
    }

    fn draw_centered(
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        x: f32,
        y: f32,
        width: f32,
        text: &str,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let text_w = ctx.measure_text_width(font, text).min(width);
        ctx.draw_text(
            layers,
            font,
            x + ((width - text_w) / 2.0).max(0.0),
            y,
            text,
            color,
            width,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_chip(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        skin: Skin,
        font: &Rc<LoadedFont>,
        label: &str,
        action: OnboardingAction,
        selected: bool,
    ) -> anyhow::Result<()> {
        let text = self.paint_chip_frame(ctx, layers, area, skin, action, selected)?;
        ctx.draw_text(
            layers,
            font,
            area.origin.x + ctx.px(CHIP_PAD_X),
            area.origin.y + ctx.px(CHIP_PAD_Y),
            label,
            text,
            area.size.width - ctx.px(CHIP_PAD_X) * 2.0,
        )
    }

    /// The shared chip surface: hit target, fill, border and focus ring.
    /// Returns the label colour so the two kinds of chip stay in step.
    fn paint_chip_frame(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        skin: Skin,
        action: OnboardingAction,
        selected: bool,
    ) -> anyhow::Result<LinearRgba> {
        self.widgets.push(area, WidgetKind::Button, action);
        let (bg, border, text) = if selected {
            // Selected still needs to answer the pointer, or the control the
            // user is already on is the one that looks dead.
            let bg = match self.state_of(action) {
                ControlState::Pressed => mix(skin.chip_selected_bg, skin.text, 0.10),
                ControlState::Hovered => mix(skin.chip_selected_bg, skin.text, 0.05),
                _ => skin.chip_selected_bg,
            };
            (bg, skin.chip_selected_border, skin.chip_selected_text)
        } else {
            match self.state_of(action) {
                ControlState::Pressed => (skin.chip_pressed_bg, skin.chip_border, skin.text),
                ControlState::Hovered => (skin.chip_hover_bg, skin.chip_border, skin.text),
                _ => (skin.chip_bg, skin.chip_border, skin.secondary_text),
            }
        };
        self.paint_ring(ctx, layers, area, skin, action, ctx.px(CHIP_RADIUS), false)?;
        ctx.draw_rounded_frame(
            layers,
            0,
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            bg,
            border,
            ctx.px(CHIP_RADIUS),
        )?;
        Ok(text)
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_button(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        skin: Skin,
        font: &Rc<LoadedFont>,
        label: &str,
        action: OnboardingAction,
        primary: bool,
    ) -> anyhow::Result<()> {
        self.widgets.push(area, WidgetKind::Button, action);
        let state = self.state_of(action);
        let (bg, border, text) = if primary {
            let bg = match state {
                ControlState::Pressed => mix(skin.primary_bg, skin.ground, 0.22),
                ControlState::Hovered => mix(skin.primary_bg, skin.ground, 0.10),
                _ => skin.primary_bg,
            };
            (bg, bg, skin.primary_text)
        } else {
            let bg = match state {
                ControlState::Pressed => skin.chip_pressed_bg,
                ControlState::Hovered => skin.chip_hover_bg,
                _ => skin.ground,
            };
            (bg, skin.chip_border, skin.secondary_text)
        };
        self.paint_ring(ctx, layers, area, skin, action, ctx.px(BTN_RADIUS), false)?;
        ctx.draw_rounded_frame(
            layers,
            0,
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            bg,
            border,
            ctx.px(BTN_RADIUS),
        )?;
        let text_w = ctx.measure_text_width(font, label).min(area.size.width);
        ctx.draw_text(
            layers,
            font,
            area.origin.x + ((area.size.width - text_w) / 2.0).max(0.0),
            area.origin.y + ctx.px(BTN_PAD_Y),
            label,
            text,
            area.size.width,
        )
    }

    /// Rings around a control, as filled rounded rects drawn *underneath* it —
    /// the exposed margin is the ring.
    ///
    /// There is no stroke primitive here, and the obvious substitute (a frame
    /// with a transparent fill) does the opposite of what it looks like: it
    /// paints the whole rect in the border colour. The selected preview came
    /// out as a solid white block that way.
    #[allow(clippy::too_many_arguments)]
    fn paint_ring(
        &self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        skin: Skin,
        action: OnboardingAction,
        radius: f32,
        selected: bool,
    ) -> anyhow::Result<()> {
        // Outermost first: focus sits outside selection so the two can show at
        // once without either being hidden.
        let focused = self.interaction.focused == Some(action);
        let mut layers_to_draw: Vec<(f32, LinearRgba)> = Vec::new();
        if focused {
            let out = ctx.px(RING_FOCUS) + if selected { ctx.px(RING_SELECTED) } else { 0.0 };
            layers_to_draw.push((out, skin.focus));
        }
        if selected {
            layers_to_draw.push((ctx.px(RING_SELECTED), skin.text));
        }
        for (out, color) in layers_to_draw {
            ctx.draw_rounded_rect(
                layers,
                0,
                area.origin.x - out,
                area.origin.y - out,
                area.size.width + out * 2.0,
                area.size.height + out * 2.0,
                color,
                radius + out,
            )?;
        }
        Ok(())
    }
}

/// The neutral skin. Everything is derived from [`UiPalette`] so both themes
/// stay correct, except the mark and the theme tiles, which depict physical
/// things and look the same either way.
#[derive(Debug, Clone, Copy)]
struct Skin {
    text: LinearRgba,
    secondary_text: LinearRgba,
    label: LinearRgba,
    chip_bg: LinearRgba,
    chip_hover_bg: LinearRgba,
    chip_pressed_bg: LinearRgba,
    chip_border: LinearRgba,
    chip_selected_bg: LinearRgba,
    chip_selected_text: LinearRgba,
    chip_selected_border: LinearRgba,
    /// Opaque page colour, for controls that should read as outline-only.
    ground: LinearRgba,
    face_border: LinearRgba,
    mark_bg: LinearRgba,
    mark_border: LinearRgba,
    primary_bg: LinearRgba,
    primary_text: LinearRgba,
    focus: LinearRgba,
}

impl Skin {
    fn new(palette: UiPalette) -> Self {
        let ground = palette.window_bg;
        // Every fill below is opaque — see `mix`. The selected chip in
        // particular sits only a little way from the ground, so the ordinary
        // text colour still reads on it; pushing it towards the text colour
        // instead would leave a near-white label on a near-white pill.
        Self {
            text: palette.text,
            secondary_text: palette.secondary_text,
            label: palette.muted_text,
            chip_bg: mix(ground, palette.text, 0.08),
            chip_hover_bg: mix(ground, palette.text, 0.13),
            chip_pressed_bg: mix(ground, palette.text, 0.20),
            chip_border: mix(ground, palette.text, 0.18),
            chip_selected_bg: mix(ground, palette.text, 0.17),
            chip_selected_text: palette.text,
            chip_selected_border: mix(ground, palette.text, 0.62),
            ground,
            face_border: mix(ground, palette.text, 0.24),
            mark_bg: mix(ground, palette.text, 0.10),
            mark_border: mix(ground, palette.text, 0.20),
            // The mono inversion: the primary action is the highest-contrast
            // thing on the page without introducing a hue.
            primary_bg: palette.text,
            primary_text: palette.window_bg,
            focus: mix(ground, palette.text, 0.65),
        }
    }
}

/// Opaque blend from `from` towards `to`, **in sRGB space**.
///
/// Two things this has to get right:
///
/// Fills passed to [`DrawContext::draw_rounded_frame`] must be opaque. That
/// helper paints the *whole* rect in the border colour and then draws the fill
/// inset by one pixel on top, so a translucent fill lets the border colour
/// flood the control: a chip whose fill was white at 24% over a border of white
/// at 55% came out pale enough that its own white label vanished into it.
///
/// And the blend must happen in sRGB, not in the linear values these colours
/// are stored as. Lerping 10% of the way from near-black to near-white in
/// *linear* space lands around 34% in sRGB — so greys meant to sit just off the
/// page came out as mid-greys.
fn mix(from: LinearRgba, to: LinearRgba, t: f32) -> LinearRgba {
    let t = t.clamp(0.0, 1.0);
    let lerp = |a: f32, b: f32| srgb_decode(srgb_encode(a) + (srgb_encode(b) - srgb_encode(a)) * t);
    LinearRgba::with_components(
        lerp(from.0, to.0),
        lerp(from.1, to.1),
        lerp(from.2, to.2),
        1.0,
    )
}

// The standard sRGB transfer function, written out rather than reached for on
// `LinearRgba`/`SrgbaTuple`: those two use different curves (one exact, one a
// gamma-2.2 approximation) and so do not round-trip. These are exact inverses,
// which is what a blend needs.
fn srgb_encode(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn srgb_decode(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[derive(Debug, Clone)]
struct OnboardingPrefs {
    space_name: String,
    target_space_id: Option<String>,
    /// Preference string from [`LANGUAGE_OPTIONS`].
    language: &'static str,
    appearance: NativeThemeMode,
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
    }
    tw.close_content_view();
}

fn apply_onboarding_preferences(settings: &mut ThinkTermNativeSettings, prefs: &OnboardingPrefs) {
    settings.appearance.theme_mode = prefs.appearance;
    // `localization.language`, not the legacy `onboarding.language`:
    // `i18n::configured_preference` reads the former first and only falls back
    // to the latter for a settings file written before localization existed.
    // Writing only the legacy field made this picker a silent no-op for anyone
    // who had ever chosen a language in the Settings window.
    settings.localization.language = Some(prefs.language.to_string());
    mark_onboarding_seen(settings);
}

/// The [`LANGUAGE_OPTIONS`] entry matching the language the app is currently
/// using, so the picker opens on the real answer rather than the legacy field.
/// Falls back to System for a preference we do not offer (a hand-edited
/// settings file, or a locale added to settings but not to the option list).
fn language_preference_for(settings: &ThinkTermNativeSettings) -> &'static str {
    let configured = crate::i18n::configured_preference(settings);
    LANGUAGE_OPTIONS
        .iter()
        .map(|option| option.preference)
        .find(|preference| preference.eq_ignore_ascii_case(configured))
        .unwrap_or(crate::i18n::SYSTEM_PREFERENCE)
}

impl ContentView for OnboardingView {
    fn title(&self) -> String {
        crate::i18n::tr("onboarding-window-title")
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
        None
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
        _cursor_on: bool,
    ) -> anyhow::Result<()> {
        self.paint_impl(ctx, layers, area, palette, font, title_font, section_font)
    }

    fn on_mouse(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse {
        self.on_mouse_impl(x, y, kind)
    }

    fn on_key(&mut self, key: KeyCode, mods: KeyModifiers) -> ContentViewResponse {
        self.on_key_impl(key, mods)
    }

    fn on_paste(&mut self, _text: &str) -> ContentViewResponse {
        ContentViewResponse::Ignored
    }

    fn on_close_requested(&mut self) -> ContentViewResponse {
        self.skip_response()
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
    use window::Appearance;

    fn prefs_with_language(language: &'static str) -> OnboardingPrefs {
        OnboardingPrefs {
            space_name: "Default".to_string(),
            target_space_id: Some("space-default".to_string()),
            language,
            appearance: NativeThemeMode::System,
        }
    }

    #[test]
    fn target_space_uses_initial_space() {
        let view = OnboardingView::new("space-default".to_string(), "Default".to_string());
        assert_eq!(view.space_name(), "Default");
        assert_eq!(view.target_space_id_for_choice(), "space-default");
    }

    /// The picker used to write only the legacy `onboarding.language`, which
    /// `configured_preference` consults *after* `localization.language` — so
    /// choosing a language here did nothing once Settings had ever set one.
    #[test]
    fn language_choice_wins_over_a_previously_configured_language() {
        let mut settings = ThinkTermNativeSettings::default();
        settings.localization.language = Some("en-US".to_string());

        apply_onboarding_preferences(&mut settings, &prefs_with_language("zh-CN"));

        assert_eq!(crate::i18n::configured_preference(&settings), "zh-CN");
    }

    #[test]
    fn language_choice_applies_to_a_fresh_settings_file() {
        let mut settings = ThinkTermNativeSettings::default();
        assert_eq!(settings.localization.language, None);

        apply_onboarding_preferences(&mut settings, &prefs_with_language("ja-JP"));

        assert_eq!(crate::i18n::configured_preference(&settings), "ja-JP");
    }

    /// Every shipped locale must be offered. The hand-written list this
    /// replaced omitted French even though `fr-FR.ftl` ships.
    #[test]
    fn every_shipped_language_is_selectable() {
        let offered: Vec<&str> = LANGUAGE_OPTIONS
            .iter()
            .map(|option| option.preference)
            .collect();
        assert!(offered.contains(&"fr-FR"));

        for preference in offered {
            let mut settings = ThinkTermNativeSettings::default();
            apply_onboarding_preferences(&mut settings, &prefs_with_language(preference));
            assert_eq!(
                crate::i18n::configured_preference(&settings),
                preference,
                "{preference} did not survive the round trip"
            );
            assert_eq!(language_preference_for(&settings), preference);
        }
    }

    #[test]
    fn an_unknown_configured_language_falls_back_to_system() {
        let mut settings = ThinkTermNativeSettings::default();
        settings.localization.language = Some("kl-GL".to_string());

        assert_eq!(
            language_preference_for(&settings),
            crate::i18n::SYSTEM_PREFERENCE
        );
    }

    /// Every control must be reachable by keyboard. The multi-step version had
    /// none: Tab switched steps, so the pills and the toggle were mouse-only.
    #[test]
    fn focus_order_covers_every_control() {
        let order = focus_order();
        assert_eq!(order.len(), LANGUAGE_OPTIONS.len() + 3 + 2);
        assert!(order.contains(&OnboardingAction::Start));
        assert!(order.contains(&OnboardingAction::Skip));
        for option in LANGUAGE_OPTIONS {
            assert!(order.contains(&OnboardingAction::Language(option.preference)));
        }
        for mode in [
            NativeThemeMode::System,
            NativeThemeMode::Light,
            NativeThemeMode::Dark,
        ] {
            assert!(order.contains(&OnboardingAction::Appearance(mode)));
        }
    }

    /// Five chips fit one row at the design width but must wrap rather than
    /// overflow when the column is narrow or a locale has long names.
    #[test]
    fn chips_wrap_instead_of_overflowing() {
        let widths = [100.0f32, 100.0, 100.0, 100.0, 100.0];

        let one_row = OnboardingView::chip_rows(&widths, 8.0, 560.0);
        assert_eq!(one_row.len(), 1);

        let narrow = OnboardingView::chip_rows(&widths, 8.0, 220.0);
        assert!(narrow.len() > 1, "expected wrapping, got {:?}", narrow);
        assert_eq!(narrow.iter().map(Vec::len).sum::<usize>(), widths.len());
        for row in &narrow {
            let used: f32 = row.iter().map(|index| widths[*index]).sum::<f32>()
                + (row.len().saturating_sub(1)) as f32 * 8.0;
            assert!(used <= 220.0, "row overflows: {}", used);
        }
    }

    /// The constants above are design pixels, which are *half* a logical
    /// pixel. Reading them as CSS pixels halves the entire layout while the
    /// text — sized by the font, not by `px()` — stays put, so the page renders
    /// crushed together and the language chips wrap a row early. That is
    /// exactly how the first cut of this screen shipped, hence the pin.
    #[test]
    fn constants_are_design_pixels_not_logical_pixels() {
        let design_dpi = if cfg!(target_os = "macos") { 144 } else { 192 };
        assert_eq!(
            crate::ui::ui_scale_for_dpi(design_dpi),
            1.0,
            "px() is 1:1 at the design dpi, so these constants are backing pixels there"
        );

        // The design surface is 2x, so halving gives the intended point size.
        assert_eq!(MARK_SIZE / 2.0, 64.0, "mark should read as 64pt");
        assert_eq!(COL_W / 2.0, 560.0, "column should read as 560pt");
        assert_eq!(TILE_W / 2.0, 112.0);
        assert_eq!(TILE_H / 2.0, 72.0);
        assert_eq!(SECTION_GAP / 2.0, 30.0);
    }

    /// The page must fit the area it is given. At the default 24-row window the
    /// content region is only ~700 backing px tall while the full-size layout
    /// wants ~1040 — and with no scrolling, no wheel handler, and dispatch
    /// gated on `py < area.max_y()`, everything past the fold was not merely
    /// clipped but unclickable. "Get Started" was off screen on first run.
    #[test]
    fn the_layout_shrinks_to_fit_a_short_window() {
        let rigid = 420.0;
        let flex = 620.0;
        let full = rigid + flex;

        // Room to spare: nothing shrinks.
        assert_eq!(fit_factor(full + 200.0, rigid, flex), 1.0);
        assert_eq!(fit_factor(full, rigid, flex), 1.0);

        // The default window: solve exactly, and check it really fits.
        let short = 700.0;
        let fit = fit_factor(short, rigid, flex);
        assert!(fit < 1.0, "should have shrunk, got {}", fit);
        assert!(
            rigid + fit * flex <= short + 0.01,
            "still overflows: {} into {}",
            rigid + fit * flex,
            short
        );

        // Absurdly short: floored rather than collapsed to nothing.
        assert_eq!(fit_factor(10.0, rigid, flex), FIT_MIN);
        // Degenerate input must not divide by zero.
        assert_eq!(fit_factor(100.0, rigid, 0.0), 1.0);
    }

    /// The theme previews once rendered "onboa…" — a second `modes` binding
    /// holding the raw key strings shadowed the translated one at the draw
    /// site. Nothing about that needs a GPU to catch.
    #[test]
    fn appearance_labels_are_translated_not_raw_keys() {
        let choices = appearance_choices();
        assert_eq!(choices.len(), 3);
        for (mode, label) in choices {
            assert!(
                !label.starts_with("onboarding-"),
                "{:?} shows a raw i18n key: {}",
                mode,
                label
            );
            assert!(!label.trim().is_empty(), "{:?} has an empty label", mode);
        }
    }

    /// `draw_rounded_frame` paints the whole rect in the border colour and then
    /// insets the fill by one pixel, so a translucent fill lets the border
    /// flood through. That is how the selected chip ended up a pale pill
    /// wearing a near-white label. Every fill the skin hands out must be
    /// opaque, and the selected fill must stay far enough from the text colour
    /// to carry it.
    #[test]
    fn skin_fills_are_opaque_and_keep_their_labels_legible() {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let palette = UiPalette::for_appearance(appearance);
            let skin = Skin::new(palette);
            let fills = [
                ("chip_bg", skin.chip_bg),
                ("chip_hover_bg", skin.chip_hover_bg),
                ("chip_pressed_bg", skin.chip_pressed_bg),
                ("chip_selected_bg", skin.chip_selected_bg),
                ("ground", skin.ground),
                ("mark_bg", skin.mark_bg),
            ];
            for (name, fill) in fills {
                assert_eq!(fill.3, 1.0, "{} is translucent on {:?}", name, appearance);
            }

            // The selected label is drawn in `chip_selected_text`. The fill has
            // to stay nearer the page than the label, or the two collapse —
            // which is what happened when the border colour flooded through a
            // translucent fill and left white text on a pale pill. Phrased
            // against the page rather than as a fraction of the gap, which was
            // slack enough to hold for any sane value.
            let luma =
                |c: LinearRgba| (srgb_encode(c.0) + srgb_encode(c.1) + srgb_encode(c.2)) / 3.0;
            let to_label = (luma(skin.chip_selected_bg) - luma(skin.chip_selected_text)).abs();
            let to_ground = (luma(skin.chip_selected_bg) - luma(skin.ground)).abs();
            assert!(
                to_label > to_ground,
                "selected fill on {:?} sits nearer its own label ({:.3}) than the page ({:.3})",
                appearance,
                to_label,
                to_ground,
            );
        }
    }

    /// `mix` blends in sRGB. Doing it on the stored linear values instead
    /// makes every "just off the page" grey land far lighter than asked for —
    /// a nominal 10% came out near 34%, which is why the mark and the buttons
    /// read as mid-grey on a near-black page.
    #[test]
    fn mix_blends_perceptually_not_in_linear_space() {
        let black = LinearRgba::with_srgba(0, 0, 0, 255);
        let white = LinearRgba::with_srgba(255, 255, 255, 255);

        let tenth = srgb_encode(mix(black, white, 0.10).0);
        assert!(
            (tenth - 0.10).abs() < 0.02,
            "10% of the way to white should be ~10% in sRGB, got {}",
            tenth
        );

        // Halfway is mid-grey to the eye, not the much lighter linear midpoint.
        let half = srgb_encode(mix(black, white, 0.5).0);
        assert!((half - 0.5).abs() < 0.02, "midpoint drifted to {}", half);

        // And the skin's own greys must stay nearer the ground than the text.
        for appearance in [Appearance::Dark, Appearance::Light] {
            let palette = UiPalette::for_appearance(appearance);
            let skin = Skin::new(palette);
            let luma =
                |c: LinearRgba| (srgb_encode(c.0) + srgb_encode(c.1) + srgb_encode(c.2)) / 3.0;
            let to_ground = (luma(skin.mark_bg) - luma(skin.ground)).abs();
            let to_text = (luma(skin.mark_bg) - luma(skin.text)).abs();
            assert!(
                to_ground < to_text,
                "mark on {:?} sits closer to the text than to the page",
                appearance
            );
        }
    }

    /// A chip wider than the column still gets its own row rather than being
    /// dropped.
    #[test]
    fn an_overlong_chip_still_gets_a_row() {
        let rows = OnboardingView::chip_rows(&[900.0, 40.0], 8.0, 200.0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec![0]);
        assert_eq!(rows[1], vec![1]);
    }
}
