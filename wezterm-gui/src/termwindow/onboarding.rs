//! First-run setup: one page, no wizard.
//!
//! It asks two things — what language, and light or dark — offers to bring
//! in what this computer already has (see `found`), and gets out of the way.
//!
//! Every surface here is a `draw_rounded_frame`, every glyph a `SvgIcon` and
//! the mark the app's own icon, which is the combination the rest of this
//! crate already draws with. An earlier cut invented its own ornament — a
//! bevelled slab with a phosphor-dot grid for the mark — by transcribing a CSS
//! mock. Those primitives have no equivalent here (no blend modes, no
//! clipping, no box-shadow) and the result did not render as designed. Prefer
//! a plainer thing that is certainly right.
//!
//! The palette is deliberately neutral. The user is choosing a theme on this
//! screen, so an accent hue would compete with the very thing being judged;
//! selection is carried by fill and border weight instead.

mod found;
mod hello;

use crate::i18n::{language_option_label, LANGUAGE_OPTIONS};
use crate::native_settings::{mark_onboarding_seen, NativeThemeMode, ThinkTermNativeSettings};
use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::content_view::{ContentView, ContentViewResponse};
use crate::termwindow::ui::icons::{BrandIcon, SvgIcon};
use crate::termwindow::TermWindow;
use crate::ui::{
    rect, ControlState, DrawContext, InteractionState, ShapedText, UiContext, UiPalette, WidgetKind,
};
use crate::utilsprites::RenderMetrics;
use fluent_bundle::FluentArgs;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wezterm_font::{GlyphInfo, LoadedFont};
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
/// Every row spans the column, label at the left edge and control at the
/// right, so the page lines up as one block under its centred heading.
const COL_W: f32 = 880.0;
/// Floor for the content column, so text never gets a zero width budget.
const MIN_COL_W: f32 = 320.0;
const SIDE_PAD: f32 = 64.0;
const SECTION_GAP: f32 = 60.0;

/// The app icon, which keeps the macOS grid's clear margin inside this.
const MARK_SIZE: f32 = 144.0;
const MARK_TO_TITLE: f32 = 40.0;
const TITLE_TO_SUB: f32 = 12.0;
const LABEL_GAP: f32 = 24.0;

const CHIP_PAD_X: f32 = 28.0;
const CHIP_PAD_Y: f32 = 18.0;
/// The language pill's chevron, after its label.
const CHEVRON: f32 = 28.0;
const CHEVRON_GAP: f32 = 16.0;

/// The open language menu.
const MENU_GAP: f32 = 8.0;
const MENU_PAD: f32 = 8.0;
const MENU_ROW_PAD_Y: f32 = 14.0;
const MENU_RADIUS: f32 = 20.0;

/// The card listing what this computer has to bring in.
const FOUND_PAD_X: f32 = 28.0;
const FOUND_PAD_Y: f32 = 22.0;
const FOUND_ICON: f32 = 48.0;
const FOUND_ICON_GAP: f32 = 24.0;
const FOUND_LINE_GAP: f32 = 4.0;
const FOUND_RADIUS: f32 = 24.0;
const FOUND_BTN_PAD_Y: f32 = 12.0;

/// The side margins hang a faint curtain of code, as in The Matrix: columns
/// of characters that stay where they are while strands of light fall
/// through them, each brightest at its head and fading up its length, all
/// thinning out towards the page so they never compete with it. It stays
/// inside the margins: nothing here clips, and past the page it would paint
/// over the sidebar.
const CURTAIN_CLEAR: f32 = 40.0;
/// Column spacing, in character widths.
const CURTAIN_PITCH: f32 = 1.7;
/// A strand moves a whole row at a time, so ten steps a second reads as
/// falling; each step redraws the whole window, which is the curtain's
/// real cost, so it takes no more than that.
const CURTAIN_FRAME: Duration = Duration::from_millis(100);
/// The intro writes its word out smoothly, so it asks for every frame.
const INTRO_FRAME: Duration = Duration::from_millis(16);
/// How long the page keeps asking whether found sessions are running.
const FOUND_WAIT: Duration = Duration::from_secs(15);
/// Characters every font has, so no strand ever shows a missing glyph.
const CURTAIN_CHARS: &[u8] = b"0123456789ABCDEFHKMTXZ:=+*<>|";

/// Appearance previews. Big enough to actually depict a light and a dark
/// surface, because that is the one thing on this page that communicates
/// without being read — the user may not have picked their language yet.
/// They widen to fill the column when it has room.
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

/// A pill: the ends are half-circles, so the radius is half the control's
/// height rather than a fixed figure. The height is settled by the font at
/// layout time -- a long translation or a larger UI size makes these taller
/// -- so a constant radius would have read as a pill at one size and as a
/// rounded rectangle at another. `pixel_snap_rounded_rect` clamps to half
/// the shorter side, so this stays correct for a control that is somehow
/// taller than it is wide.
fn pill_radius(area: RectF) -> f32 {
    area.size.height / 2.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum OnboardingAction {
    Start,
    Skip,
    /// The language pill, which opens and closes its menu.
    LanguageMenu,
    /// A row of the open language menu. Carries the stable preference string
    /// from [`LANGUAGE_OPTIONS`], which is what `localization.language`
    /// persists, rather than the legacy `NativeLanguagePreference` enum that
    /// cannot name every shipped locale.
    Language(&'static str),
    /// Anywhere outside the open menu: closes it without reaching what is
    /// underneath.
    CloseMenu,
    Appearance(NativeThemeMode),
    /// An Import button: the sessions of the import source with this id, or
    /// `None` for the WezTerm configuration.
    Import(Option<&'static str>),
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
/// was reachable without pointing at it. The languages are reached through
/// their pill, whose menu takes the arrow keys.
fn focus_order(found: &found::Found) -> Vec<OnboardingAction> {
    let mut order = vec![OnboardingAction::LanguageMenu];
    order.extend(
        [
            NativeThemeMode::System,
            NativeThemeMode::Light,
            NativeThemeMode::Dark,
        ]
        .map(OnboardingAction::Appearance),
    );
    order.extend(
        found
            .sessions
            .iter()
            .map(|sessions| OnboardingAction::Import(Some(sessions.id))),
    );
    if found.wezterm_config {
        order.push(OnboardingAction::Import(None));
    }
    if found.editors.is_some() {
        order.push(OnboardingAction::Import(Some(
            crate::settings_window::EDITORS_SOURCE,
        )));
    }
    order.push(OnboardingAction::Skip);
    order.push(OnboardingAction::Start);
    order
}

/// What heads a row of the found card.
enum FoundMark {
    /// A program's own mark, or a terminal where it has none.
    Program(Option<BrandIcon>),
    /// Editors' marks on overlapping tiles.
    Editors(Vec<crate::editor_projects::EditorKind>),
}

/// SplitMix64 over a pair: the curtain's fixed pattern.
fn curtain_hash(a: u64, b: u64) -> u64 {
    let mut z = a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b.wrapping_add(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// One strand of light falling down a column, round and round.
struct Strand {
    seed: u64,
    /// Rows a second.
    speed: f32,
    /// Rows from one pass's head to the next: the page, the longest strand,
    /// and a pause before it comes round again.
    cycle: f32,
    /// Where in its cycle it is at the start.
    offset: f32,
}

struct CurtainColumn {
    x: f32,
    /// Fainter nearer the page.
    outward: f32,
    seed: u64,
    strands: Vec<Strand>,
}

/// The curtain as laid out for the current page, column, font and theme,
/// with this frame's characters and what they shaped to. Bounded by the
/// window's size and the character set; it goes with the page.
struct Curtain {
    key: [u32; 15],
    font: usize,
    top: f32,
    line_h: f32,
    rows: usize,
    ground: LinearRgba,
    ink: LinearRgba,
    columns: Vec<CurtainColumn>,
    glyphs: Vec<(char, f32, f32, LinearRgba)>,
    /// How lit each row of the column being laid out is: where two strands
    /// cross, the brighter one wins rather than both being drawn.
    lit_rows: Vec<f32>,
    shaped: HashMap<char, Option<GlyphInfo>>,
}

impl Curtain {
    /// Columns in both side margins, counted out from the page. The pattern
    /// comes from each column's place: the same page lays out the same
    /// curtain, and a wider window only adds columns at the outer edge. A
    /// margin with room for fewer than two columns gets none.
    #[allow(clippy::too_many_arguments)]
    fn lay_out(
        area: RectF,
        col_x: f32,
        col_w: f32,
        clear: f32,
        char_w: f32,
        line_h: f32,
        ground: LinearRgba,
        ink: LinearRgba,
    ) -> Self {
        let mut curtain = Self {
            key: [0; 15],
            font: 0,
            top: area.origin.y,
            line_h,
            rows: 0,
            ground,
            ink,
            columns: Vec::new(),
            glyphs: Vec::new(),
            lit_rows: Vec::new(),
            shaped: HashMap::new(),
        };
        if char_w <= 0.0 || line_h <= 0.0 {
            return curtain;
        }
        let pitch = (char_w * CURTAIN_PITCH).round();
        let rows = (area.size.height / line_h).floor() as i64;
        let room = col_x - area.origin.x - clear - pitch / 2.0;
        let count = (room / pitch).floor() as i64;
        if count < 2 || rows < 2 {
            return curtain;
        }
        for side in 0..2u64 {
            for column in 0..count {
                let x = if side == 0 {
                    col_x - clear - (column + 1) as f32 * pitch
                } else {
                    col_x + col_w + clear + column as f32 * pitch
                };
                let seed = curtain_hash(side, column as u64);
                // Now and then a column hangs nothing, so it reads as strands
                // rather than as a wall of text.
                if seed % 7 == 0 {
                    continue;
                }
                let strands = (0..1 + (seed >> 8) % 3)
                    .map(|strand| {
                        let seed = curtain_hash(seed, strand + 1);
                        let pause = (seed >> 24) % (rows as u64 / 2 + 1);
                        let cycle = (rows as u64 + 32 + pause) as f32;
                        Strand {
                            seed,
                            speed: 5.0 + (seed >> 40) as f32 % 8.0,
                            cycle,
                            offset: (seed >> 16) as f32 % cycle,
                        }
                    })
                    .collect();
                curtain.columns.push(CurtainColumn {
                    x: x + (pitch - char_w) / 2.0,
                    outward: 0.3 + 0.7 * (column as f32 / (count - 1) as f32),
                    seed,
                    strands,
                });
            }
        }
        curtain.rows = rows as usize;
        curtain.lit_rows = vec![0.0; rows as usize];
        curtain
    }

    /// The characters lit `elapsed` seconds in, rebuilt in place. Each
    /// strand's head moves a whole row at a time and its length changes
    /// from one pass to the next; now and then a character turns into
    /// another, each on its own beat.
    fn frame(&mut self, elapsed: f32) {
        self.glyphs.clear();
        let rows = self.rows as i64;
        for column in &self.columns {
            self.lit_rows.iter_mut().for_each(|lit| *lit = 0.0);
            for strand in &column.strands {
                let travelled = strand.offset + elapsed * strand.speed;
                let pass = (travelled / strand.cycle).floor() as u64;
                let head = (travelled % strand.cycle).floor() as i64;
                let length = 8 + (curtain_hash(strand.seed, pass) % 24) as i64;
                for step in 0..length {
                    let row = head - step;
                    if row < 0 || row >= rows {
                        continue;
                    }
                    let fade = 1.0 - step as f32 / length as f32;
                    let mut strength = 0.02 + 0.09 * fade.powf(1.5);
                    if step == 0 {
                        strength += 0.035;
                    }
                    let lit = &mut self.lit_rows[row as usize];
                    *lit = lit.max(strength);
                }
            }
            for (row, strength) in self.lit_rows.iter().enumerate() {
                if *strength <= 0.0 {
                    continue;
                }
                let place = curtain_hash(column.seed, row as u64);
                let beat = (elapsed * 0.6 + (place >> 32) as f32 % 7.0).floor() as u64;
                let pick = curtain_hash(place, beat) as usize % CURTAIN_CHARS.len();
                let row = row as i64;
                // Eased in at the top and bottom of the page rather than cut
                // off by them.
                let edge = (row.min(rows - 1 - row) as f32 / 3.0).min(1.0);
                self.glyphs.push((
                    CURTAIN_CHARS[pick] as char,
                    column.x,
                    self.top + row as f32 * self.line_h,
                    mix(self.ground, self.ink, strength * column.outward * edge),
                ));
            }
        }
    }
}

fn language_index(preference: &str) -> usize {
    LANGUAGE_OPTIONS
        .iter()
        .position(|option| option.preference == preference)
        .unwrap_or(0)
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
    /// What this computer has to bring in.
    found: found::Found,
    /// The language menu while it is open: the row the keyboard is on.
    menu: Option<usize>,
    /// When the page opened, which bounds the wait for the running check.
    opened: Instant,
    curtain: Option<Curtain>,
    /// "hello." written out before the page appears. Gone once the page is
    /// fully in; until then the page takes no input, since none of it can
    /// be seen yet.
    intro: Option<hello::Intro>,
    /// What this page's strings shaped to. The curtain repaints the page
    /// many times a second, and its text never changes in between.
    shaped: ShapedText,
}

impl OnboardingView {
    pub(crate) fn new(initial_space_id: String, initial_space_name: String) -> Self {
        Self {
            intro: Some(hello::Intro::new()),
            ..Self::with_found(initial_space_id, initial_space_name, found::Found::look())
        }
    }

    fn with_found(
        initial_space_id: String,
        initial_space_name: String,
        found: found::Found,
    ) -> Self {
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
            found,
            menu: None,
            opened: Instant::now(),
            curtain: None,
            intro: None,
            shaped: ShapedText::default(),
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
                    // The pointer and the arrow keys share one highlight.
                    if let (Some(_), Some(OnboardingAction::Language(language))) = (self.menu, hit)
                    {
                        self.menu = Some(language_index(language));
                    }
                    ContentViewResponse::Redraw
                } else {
                    ContentViewResponse::Ignored
                }
            }
            WMEK::Press(MousePress::Left) => {
                self.interaction.pressed = hit;
                // Clicking also takes focus, so Tab continues from where the
                // pointer left off rather than jumping back to the start. A
                // language is reached through its pill.
                match hit {
                    Some(OnboardingAction::CloseMenu) | None => {}
                    Some(OnboardingAction::Language(_)) => {
                        self.interaction.focused = Some(OnboardingAction::LanguageMenu)
                    }
                    Some(action) => self.interaction.focused = Some(action),
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
            OnboardingAction::Import(source) => self.import_response(source),
            OnboardingAction::LanguageMenu => {
                self.menu = match self.menu {
                    Some(_) => None,
                    None => Some(language_index(self.selected_language)),
                };
                ContentViewResponse::Redraw
            }
            OnboardingAction::CloseMenu => {
                self.menu = None;
                ContentViewResponse::Redraw
            }
            OnboardingAction::Language(language) => {
                self.menu = None;
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
        self.menu = None;
        let order = focus_order(&self.found);
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
        if let Some(row) = self.menu {
            let rows = LANGUAGE_OPTIONS.len();
            return match (key, mods) {
                // Escape closes the menu, not the page.
                (KeyCode::Escape, _) => self.apply(OnboardingAction::CloseMenu),
                (KeyCode::UpArrow, _) => {
                    self.menu = Some((row + rows - 1) % rows);
                    ContentViewResponse::Redraw
                }
                (KeyCode::DownArrow, _) => {
                    self.menu = Some((row + 1) % rows);
                    ContentViewResponse::Redraw
                }
                (KeyCode::Enter, _) | (KeyCode::Char(' '), _) => {
                    self.apply(OnboardingAction::Language(LANGUAGE_OPTIONS[row].preference))
                }
                (KeyCode::Tab, KeyModifiers::NONE) => self.move_focus(true),
                (KeyCode::Tab, KeyModifiers::SHIFT) => self.move_focus(false),
                _ => ContentViewResponse::Ignored,
            };
        }
        let focused = self.interaction.focused;
        match (key, mods) {
            (KeyCode::Escape, _) => self.skip_response(),
            (KeyCode::Tab, KeyModifiers::NONE) => self.move_focus(true),
            (KeyCode::Tab, KeyModifiers::SHIFT) => self.move_focus(false),
            (KeyCode::DownArrow, _) | (KeyCode::UpArrow, _)
                if focused == Some(OnboardingAction::LanguageMenu) =>
            {
                self.apply(OnboardingAction::LanguageMenu)
            }
            // Space activates whatever is focused. Enter is the default
            // action — it starts — except on a control that has an action of
            // its own: Skip, the language pill, an Import button. A theme
            // is applied the moment it is picked, so there is nothing left
            // for Enter to confirm there.
            (KeyCode::Char(' '), _) => match focused {
                Some(action) => self.apply(action),
                None => ContentViewResponse::Ignored,
            },
            (KeyCode::Enter, _) => match focused {
                Some(
                    action @ (OnboardingAction::Skip
                    | OnboardingAction::LanguageMenu
                    | OnboardingAction::Import(_)),
                ) => self.apply(action),
                _ => self.finish_response(),
            },
            _ => ContentViewResponse::Ignored,
        }
    }

    fn prefs(&self) -> OnboardingPrefs {
        OnboardingPrefs {
            space_name: self.space_name(),
            target_space_id: Some(self.target_space_id_for_choice()),
            language: self.selected_language,
            appearance: self.selected_appearance,
        }
    }

    fn finish_response(&self) -> ContentViewResponse {
        let prefs = self.prefs();
        ContentViewResponse::Run(Box::new(move |tw: &mut TermWindow| {
            finish_onboarding(tw, prefs);
        }))
    }

    /// Import keeps what was picked here, as Get Started does, and goes on to
    /// Settings' Import page with the source chosen and its first step taken.
    fn import_response(&self, source: Option<&'static str>) -> ContentViewResponse {
        let prefs = self.prefs();
        ContentViewResponse::Run(Box::new(move |tw: &mut TermWindow| {
            let space_id = finish_onboarding(tw, prefs);
            crate::settings_window::show_import_from(tw.mux_window_id, &space_id, source);
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

    fn paint_impl(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        palette: UiPalette,
        font: &Rc<LoadedFont>,
        title_font: &Rc<LoadedFont>,
        section_font: &Rc<LoadedFont>,
        caption_font: &Rc<LoadedFont>,
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
        self.paint_curtain(ctx, layers, area, col_x, col_w, skin, caption_font)?;

        // --- measure everything first so the column can be centred vertically
        let languages: Vec<String> = LANGUAGE_OPTIONS
            .iter()
            .map(|option| language_option_label(*option))
            .collect();
        // As wide as the longest name, so the pill keeps its size whichever
        // is chosen and the open menu lines up under it.
        let pill_w = (languages
            .iter()
            .map(|label| self.width(ctx, font, label))
            .fold(0.0f32, f32::max)
            + ctx.px(CHIP_PAD_X) * 2.0
            + ctx.px(CHEVRON_GAP)
            + ctx.px(CHEVRON))
        .ceil()
        .min(col_w);
        let language_label = crate::i18n::tr("onboarding-language");
        // A column too narrow for the label and the pill side by side puts
        // the pill under its label instead.
        let language_stacked =
            self.width(ctx, section_font, &language_label) + ctx.px(LABEL_GAP) + pill_w > col_w;

        let modes = appearance_choices();
        // A preview is at least as wide as the widest caption. At the design
        // width the French "Suivre le système" was ellipsised to "Suivre le
        // syst…", which is the one label a user who cannot yet read the UI
        // most needs whole.
        let widest_caption = modes
            .iter()
            .map(|(_, label)| self.width(ctx, font, label))
            .fold(0.0f32, f32::max);
        let tile_min = ctx
            .px(TILE_W)
            .max(widest_caption + ctx.px(TILE_CAPTION_PAD));
        let tile_gap = ctx.px(TILE_GAP);
        let tile_count = modes.len() as f32;
        // The selected tile's ring is drawn outside it. Inset by its weight,
        // the ring is what meets the column's edges, and the language menu,
        // which ends at the right edge, covers it whole.
        let tiles_x = col_x + ctx.px(RING_SELECTED);
        let tiles_w = col_w - ctx.px(RING_SELECTED) * 2.0;
        let tiles_fit = tile_min * tile_count + tile_gap * (tile_count - 1.0) <= tiles_w;
        let tile_w = if tiles_fit {
            (tiles_w - tile_gap * (tile_count - 1.0)) / tile_count
        } else {
            tile_min.min(tiles_w)
        };

        let show_found = !self.found.is_empty();
        let found_rows = self.found.rows() as f32;

        let privacy = crate::i18n::tr("onboarding-privacy");
        let start_label = crate::i18n::tr("onboarding-start");
        let skip_label = crate::i18n::tr("onboarding-skip");
        // Rounded up: a label drawn into exactly its measured width loses its
        // last glyph to any rounding in between.
        let start_w = (self.width(ctx, font, &start_label) + ctx.px(BTN_PAD_X) * 2.0).ceil();
        let skip_w = (self.width(ctx, font, &skip_label) + ctx.px(BTN_PAD_X) * 2.0).ceil();
        let buttons_w = skip_w + ctx.px(BTN_GAP) + start_w;
        // The privacy line shares the buttons' row when it fits beside them.
        let footer_inline = self.width(ctx, font, &privacy) + ctx.px(BTN_GAP) + buttons_w <= col_w;

        // Text cannot shrink; spacing and decoration can. Page height is
        // therefore linear in a single factor, so solve it rather than guess:
        // at the default 24-row window the content area is only ~700px tall and
        // the full-size layout runs ~340px past it — with no scrolling and no
        // wheel handler, that put "Get Started" off screen and out of reach.
        let tile_rows = if tiles_fit { 1.0 } else { tile_count };
        let mut rigid = title_h
            + body_h
            + if language_stacked {
                label_h + body_h
            } else {
                body_h
            }
            + label_h
            + tile_rows * body_h
            + body_h
            + body_h;
        let mut flex = TITLE_TO_SUB
            + CHIP_PAD_Y * 2.0
            + if language_stacked { LABEL_GAP } else { 0.0 }
            + LABEL_GAP
            + tile_rows * (TILE_H + TILE_LABEL_GAP)
            + (tile_rows - 1.0).max(0.0) * TILE_GAP
            + TILE_LABEL_GAP
            + BTN_PAD_Y * 2.0
            + SECTION_GAP * 3.0;
        if show_found {
            rigid += label_h + found_rows * body_h * 2.0;
            flex += LABEL_GAP + found_rows * (FOUND_LINE_GAP + FOUND_PAD_Y * 2.0) + SECTION_GAP;
        }
        if !footer_inline {
            rigid += body_h;
            flex += LABEL_GAP;
        }
        let available = area.size.height - ctx.px(SIDE_PAD) * 2.0;
        // The mark is decoration: a window too short for the page even at its
        // tightest spacing loses it before anything that can be clicked.
        let show_mark = rigid + ctx.px(flex + MARK_SIZE + MARK_TO_TITLE) * FIT_MIN <= available;
        if show_mark {
            flex += MARK_SIZE + MARK_TO_TITLE;
        }
        let fit = fit_factor(available, rigid, ctx.px(flex));
        // Every discretionary dimension goes through this from here on.
        let fx = |value: f32| ctx.px(value) * fit;

        let chip_h = body_h + fx(CHIP_PAD_Y) * 2.0;
        let language_h = if language_stacked {
            label_h + fx(LABEL_GAP) + chip_h
        } else {
            label_h.max(chip_h)
        };
        let tile_h = fx(TILE_H);
        // The light-theme footnote is only drawn for Follow System and Light,
        // but its room is reserved either way: the page is vertically centred,
        // so letting the height depend on the selection would make everything
        // above and below jump as the user tries the three tiles.
        let note_h = fx(TILE_LABEL_GAP) + body_h;
        let tiles_h = tile_rows * (tile_h + fx(TILE_LABEL_GAP) + body_h)
            + (tile_rows - 1.0).max(0.0) * fx(TILE_GAP);
        let modes_h = label_h + fx(LABEL_GAP) + tiles_h + note_h;
        let found_row_h = body_h * 2.0 + fx(FOUND_LINE_GAP) + fx(FOUND_PAD_Y) * 2.0;
        let found_h = label_h + fx(LABEL_GAP) + found_rows * found_row_h;
        let actions_h = body_h + fx(BTN_PAD_Y) * 2.0;
        let footer_h = if footer_inline {
            actions_h
        } else {
            body_h + fx(LABEL_GAP) + actions_h
        };
        let mark_h = if show_mark {
            fx(MARK_SIZE) + fx(MARK_TO_TITLE)
        } else {
            0.0
        };
        let brand_h = mark_h + title_h + fx(TITLE_TO_SUB) + body_h;

        let gap = fx(SECTION_GAP);
        let total_h = brand_h
            + gap
            + language_h
            + gap
            + modes_h
            + gap
            + if show_found { found_h + gap } else { 0.0 }
            + footer_h;

        let mut y = area.origin.y + ((area.size.height - total_h) / 2.0).max(0.0);

        // --- brand
        if show_mark {
            ctx.draw_app_icon(
                layers,
                col_x + (col_w - fx(MARK_SIZE)) / 2.0,
                y,
                fx(MARK_SIZE),
            )?;
            y += mark_h;
        }

        let title = crate::i18n::tr("onboarding-title");
        self.draw_centered(
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
        self.draw_centered(
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

        // --- language: the label at the left, the pill at the right
        let pill = if language_stacked {
            self.text(
                ctx,
                layers,
                section_font,
                col_x,
                y,
                &language_label,
                skin.label,
                col_w,
            )?;
            rect(col_x, y + label_h + fx(LABEL_GAP), pill_w, chip_h)
        } else {
            self.text(
                ctx,
                layers,
                section_font,
                col_x,
                y + (language_h - label_h) / 2.0,
                &language_label,
                skin.label,
                col_w - pill_w - ctx.px(LABEL_GAP),
            )?;
            rect(
                col_x + col_w - pill_w,
                y + (language_h - chip_h) / 2.0,
                pill_w,
                chip_h,
            )
        };
        let selected_language = &languages[language_index(self.selected_language)];
        self.paint_language_pill(ctx, layers, pill, skin, font, selected_language)?;
        y += language_h + gap;

        // --- appearance
        self.text(
            ctx,
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
                (tiles_x + index as f32 * (tile_w + tile_gap), y)
            } else {
                (
                    tiles_x + (tiles_w - tile_w) / 2.0,
                    y + index as f32 * tile_step,
                )
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

        // --- light-theme footnote; its space is reserved in modes_h either way
        if matches!(
            self.selected_appearance,
            NativeThemeMode::System | NativeThemeMode::Light
        ) {
            self.draw_centered(
                ctx,
                layers,
                font,
                col_x,
                y + tiles_h + fx(TILE_LABEL_GAP),
                col_w,
                &crate::i18n::tr("onboarding-theme-light-note"),
                palette.muted_text,
            )?;
        }
        y += tiles_h + note_h + gap;

        // --- what this computer has to bring in
        if show_found {
            self.text(
                ctx,
                layers,
                section_font,
                col_x,
                y,
                &crate::i18n::tr("onboarding-found-title"),
                skin.label,
                col_w,
            )?;
            y += label_h + fx(LABEL_GAP);
            self.paint_found(
                ctx,
                layers,
                rect(col_x, y, col_w, found_rows * found_row_h),
                skin,
                font,
                found_row_h,
                fit,
            )?;
            y += found_rows * found_row_h + gap;
        }

        // --- privacy footnote and the actions, right aligned
        let buttons_y = if footer_inline {
            self.text(
                ctx,
                layers,
                font,
                col_x,
                y + (actions_h - body_h) / 2.0,
                &privacy,
                palette.muted_text,
                col_w - buttons_w - ctx.px(BTN_GAP),
            )?;
            y
        } else {
            self.text(
                ctx,
                layers,
                font,
                col_x,
                y,
                &privacy,
                palette.muted_text,
                col_w,
            )?;
            y + body_h + fx(LABEL_GAP)
        };
        let start_x = col_x + col_w - start_w;
        self.paint_button(
            ctx,
            layers,
            rect(start_x, buttons_y, start_w, actions_h),
            skin,
            font,
            &start_label,
            OnboardingAction::Start,
            true,
        )?;
        self.paint_button(
            ctx,
            layers,
            rect(
                start_x - ctx.px(BTN_GAP) - skip_w,
                buttons_y,
                skip_w,
                actions_h,
            ),
            skin,
            font,
            &skip_label,
            OnboardingAction::Skip,
            false,
        )?;

        // --- last, so it covers the page and takes its clicks
        if let Some(highlight) = self.menu {
            self.paint_language_menu(
                ctx, layers, area, pill, skin, font, &languages, highlight, fit,
            )?;
        }

        Ok(())
    }

    /// The curtain in both side margins. Its columns are laid out again only
    /// when something they depend on changes -- the page, the column, the
    /// font or the theme; each frame only moves the strands along them.
    fn paint_curtain(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        col_x: f32,
        col_w: f32,
        skin: Skin,
        font: &Rc<LoadedFont>,
    ) -> anyhow::Result<()> {
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let ctx = ctx.with_metrics(&metrics);
        let char_w = metrics.cell_size.width as f32;
        let line_h = metrics.cell_size.height as f32;
        let clear = ctx.px(CURTAIN_CLEAR);
        let key = [
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            col_x,
            col_w,
            clear,
            char_w,
            line_h,
            skin.ground.0,
            skin.ground.1,
            skin.ground.2,
            skin.text.0,
            skin.text.1,
            skin.text.2,
        ]
        .map(f32::to_bits);
        let font_id = font.id();
        let current = self
            .curtain
            .as_ref()
            .is_some_and(|curtain| curtain.key == key && curtain.font == font_id);
        if !current {
            // A new layout in the same font keeps what its characters shaped
            // to; another font starts over.
            let shaped = match self.curtain.take() {
                Some(old) if old.font == font_id => old.shaped,
                _ => HashMap::new(),
            };
            // A green that only shows as a tint at this strength.
            let ink = mix(
                skin.text,
                LinearRgba::with_srgba(0x3D, 0xDC, 0x84, 255),
                0.45,
            );
            let mut curtain =
                Curtain::lay_out(area, col_x, col_w, clear, char_w, line_h, skin.ground, ink);
            curtain.key = key;
            curtain.font = font_id;
            curtain.shaped = shaped;
            self.curtain = Some(curtain);
        }
        let curtain = self.curtain.as_mut().unwrap();
        curtain.frame(self.opened.elapsed().as_secs_f32());
        ctx.draw_glyphs_on_layer(layers, 0, font, &curtain.glyphs, &mut curtain.shaped)
    }

    /// The closed language menu: the chosen language and a chevron.
    fn paint_language_pill(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        skin: Skin,
        font: &Rc<LoadedFont>,
        label: &str,
    ) -> anyhow::Result<()> {
        let open = self.menu.is_some();
        let text = self.paint_chip_frame(
            ctx,
            layers,
            area,
            skin,
            OnboardingAction::LanguageMenu,
            open,
        )?;
        let chevron = ctx.px(CHEVRON);
        self.text(
            ctx,
            layers,
            font,
            area.origin.x + ctx.px(CHIP_PAD_X),
            area.origin.y + (area.size.height - Self::text_h(font)) / 2.0,
            label,
            text,
            area.size.width - ctx.px(CHIP_PAD_X) * 2.0 - ctx.px(CHEVRON_GAP) - chevron,
        )?;
        ctx.draw_svg_icon(
            layers,
            SvgIcon::ChevronDown,
            area.max_x() - ctx.px(CHIP_PAD_X) - chevron,
            area.origin.y + (area.size.height - chevron) / 2.0,
            chevron,
            skin.secondary_text,
        )
    }

    /// The open language menu, under its pill or above it when the page has
    /// no room below. All of it is on the top layer, after everything else,
    /// so the controls it covers neither show through nor take its clicks.
    #[allow(clippy::too_many_arguments)]
    fn paint_language_menu(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: RectF,
        pill: RectF,
        skin: Skin,
        font: &Rc<LoadedFont>,
        languages: &[String],
        highlight: usize,
        fit: f32,
    ) -> anyhow::Result<()> {
        let fx = |value: f32| ctx.px(value) * fit;
        let body_h = Self::text_h(font);
        let pad = fx(MENU_PAD);
        let row_h = body_h + fx(MENU_ROW_PAD_Y) * 2.0;
        let menu_h = row_h * languages.len() as f32 + pad * 2.0;
        let below = pill.max_y() + fx(MENU_GAP);
        let menu_y = if below + menu_h <= area.max_y() {
            below
        } else {
            (pill.origin.y - fx(MENU_GAP) - menu_h).max(area.origin.y)
        };
        let menu = rect(pill.origin.x, menu_y, pill.size.width, menu_h);

        self.widgets
            .push(area, WidgetKind::Button, OnboardingAction::CloseMenu);
        ctx.draw_elevated_surface(
            layers,
            2,
            menu,
            skin.menu_bg,
            skin.chip_border,
            skin.shadow,
            fx(MENU_RADIUS),
        )?;
        let chevron = ctx.px(CHEVRON);
        for (index, label) in languages.iter().enumerate() {
            let preference = LANGUAGE_OPTIONS[index].preference;
            let action = OnboardingAction::Language(preference);
            let row = rect(
                menu.origin.x + pad,
                menu.origin.y + pad + index as f32 * row_h,
                menu.size.width - pad * 2.0,
                row_h,
            );
            self.widgets.push(row, WidgetKind::Button, action);
            if index == highlight {
                ctx.draw_rounded_rect(
                    layers,
                    2,
                    row.origin.x,
                    row.origin.y,
                    row.size.width,
                    row.size.height,
                    skin.chip_hover_bg,
                    (fx(MENU_RADIUS) - pad).max(0.0),
                )?;
            }
            let selected = self.selected_language == preference;
            // Lined up with the pill's own label and chevron.
            ctx.draw_text_shaped_on_layer(
                layers,
                2,
                font,
                pill.origin.x + ctx.px(CHIP_PAD_X),
                row.origin.y + (row_h - body_h) / 2.0,
                label,
                if selected {
                    skin.text
                } else {
                    skin.secondary_text
                },
                pill.size.width - ctx.px(CHIP_PAD_X) * 2.0 - ctx.px(CHEVRON_GAP) - chevron,
                &mut self.shaped,
            )?;
            if selected {
                ctx.draw_svg_icon(
                    layers,
                    SvgIcon::Check,
                    pill.max_x() - ctx.px(CHIP_PAD_X) - chevron,
                    row.origin.y + (row_h - chevron) / 2.0,
                    chevron,
                    skin.text,
                )?;
            }
        }
        Ok(())
    }

    /// One row per thing this computer has to bring in: its mark, what it
    /// is, and a button that opens Settings' Import page on it.
    #[allow(clippy::too_many_arguments)]
    fn paint_found(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        card: RectF,
        skin: Skin,
        font: &Rc<LoadedFont>,
        row_h: f32,
        fit: f32,
    ) -> anyhow::Result<()> {
        let fx = |value: f32| ctx.px(value) * fit;
        let body_h = Self::text_h(font);
        ctx.draw_rounded_frame(
            layers,
            0,
            card.origin.x,
            card.origin.y,
            card.size.width,
            card.size.height,
            skin.chip_bg,
            skin.chip_border,
            fx(FOUND_RADIUS),
        )?;

        let mut rows: Vec<(FoundMark, String, String, String, OnboardingAction)> = self
            .found
            .sessions
            .iter()
            .map(|sessions| {
                let mut args = FluentArgs::new();
                args.set("count", sessions.count as i64);
                let detail = match sessions.running() {
                    Some(running) if running > 0 => {
                        args.set("running", running as i64);
                        crate::i18n::tr_args("onboarding-found-sessions-running", &args)
                    }
                    _ => crate::i18n::tr_args("onboarding-found-sessions", &args),
                };
                (
                    FoundMark::Program(sessions.icon),
                    sessions.name.to_string(),
                    detail,
                    crate::i18n::tr("onboarding-found-import"),
                    OnboardingAction::Import(Some(sessions.id)),
                )
            })
            .collect();
        if self.found.wezterm_config {
            rows.push((
                FoundMark::Program(Some(BrandIcon::WezTerm)),
                "WezTerm".to_string(),
                crate::i18n::tr("onboarding-found-wezterm"),
                crate::i18n::tr("onboarding-found-import-settings"),
                OnboardingAction::Import(None),
            ));
        }
        if let Some(editors) = &self.found.editors {
            rows.push((
                FoundMark::Editors(editors.kinds.clone()),
                editors.names.clone(),
                crate::i18n::tr("onboarding-found-editors"),
                crate::i18n::tr("onboarding-found-choose-projects"),
                OnboardingAction::Import(Some(crate::settings_window::EDITORS_SOURCE)),
            ));
        }

        let pad_x = fx(FOUND_PAD_X);
        let icon = fx(FOUND_ICON);
        // A row of editors is wider than one mark; every mark is centred
        // in the widest, so the names still line up.
        let mark_width = |mark: &FoundMark| match mark {
            FoundMark::Program(_) => icon,
            FoundMark::Editors(kinds) => crate::editor_projects::stack_width(kinds.len(), icon),
        };
        let column = rows
            .iter()
            .map(|(mark, ..)| mark_width(mark))
            .fold(icon, f32::max);
        for (index, (mark, name, detail, button, action)) in rows.iter().enumerate() {
            let top = card.origin.y + index as f32 * row_h;
            if index > 0 {
                ctx.draw_rect(
                    layers,
                    0,
                    card.origin.x + pad_x,
                    top,
                    card.size.width - pad_x * 2.0,
                    ctx.px(2.0).max(1.0),
                    skin.chip_border,
                )?;
            }
            let column_x = card.origin.x + pad_x;
            let icon_x = column_x + (column - mark_width(mark)) / 2.0;
            let icon_y = top + (row_h - icon) / 2.0;
            match mark {
                FoundMark::Program(Some(brand)) => crate::ui::tile::draw_brand_tile(
                    ctx,
                    layers,
                    *brand,
                    icon_x,
                    icon_y,
                    icon,
                    skin.dark,
                    &crate::settings_window::ROW_TILE,
                )?,
                FoundMark::Program(None) => ctx.draw_svg_icon(
                    layers,
                    SvgIcon::Terminal,
                    icon_x,
                    icon_y,
                    icon,
                    skin.secondary_text,
                )?,
                FoundMark::Editors(kinds) => crate::editor_projects::draw_stack(
                    ctx,
                    layers,
                    kinds,
                    icon_x,
                    icon_y,
                    icon,
                    skin.chip_bg,
                    skin.dark,
                )?,
            }

            let button_w = (self.width(ctx, font, button) + ctx.px(BTN_PAD_X) * 2.0).ceil();
            let button_h = body_h + fx(FOUND_BTN_PAD_Y) * 2.0;
            let button_x = card.max_x() - pad_x - button_w;
            self.paint_button(
                ctx,
                layers,
                rect(button_x, top + (row_h - button_h) / 2.0, button_w, button_h),
                skin,
                font,
                button,
                *action,
                false,
            )?;

            let text_x = column_x + column + fx(FOUND_ICON_GAP);
            let text_w = (button_x - fx(FOUND_ICON_GAP) - text_x).max(1.0);
            let text_y = top + (row_h - body_h * 2.0 - fx(FOUND_LINE_GAP)) / 2.0;
            self.text(ctx, layers, font, text_x, text_y, name, skin.text, text_w)?;
            self.text(
                ctx,
                layers,
                font,
                text_x,
                text_y + body_h + fx(FOUND_LINE_GAP),
                detail,
                skin.secondary_text,
                text_w,
            )?;
        }
        Ok(())
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
            // Onboarding offers three tiles and builds the list itself, so
            // `FollowTerminal` never reaches here; drawn as dark rather than
            // left to panic if that list ever grows.
            NativeThemeMode::Dark | NativeThemeMode::FollowTerminal => {
                self.paint_face(ctx, layers, inner, skin, false, fit)?
            }
        }

        let text_w = self.width(ctx, font, label).min(tile.size.width);
        self.text(
            ctx,
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

    /// `ctx.measure_text_width` through this page's shaping cache.
    fn width(&mut self, ctx: &DrawContext, font: &Rc<LoadedFont>, text: &str) -> f32 {
        ctx.measure_text_width_shaped(font, text, &mut self.shaped)
    }

    /// `ctx.draw_text` through this page's shaping cache.
    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        x: f32,
        y: f32,
        text: &str,
        color: LinearRgba,
        max_width: f32,
    ) -> anyhow::Result<()> {
        ctx.draw_text_shaped_on_layer(
            layers,
            1,
            font,
            x,
            y,
            text,
            color,
            max_width,
            &mut self.shaped,
        )
    }

    fn draw_centered(
        &mut self,
        ctx: &DrawContext,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        x: f32,
        y: f32,
        width: f32,
        text: &str,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let text_w = self.width(ctx, font, text).min(width);
        self.text(
            ctx,
            layers,
            font,
            x + ((width - text_w) / 2.0).max(0.0),
            y,
            text,
            color,
            width,
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
        self.paint_ring(ctx, layers, area, skin, action, pill_radius(area), false)?;
        ctx.draw_rounded_frame(
            layers,
            0,
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            bg,
            border,
            pill_radius(area),
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
        self.paint_ring(ctx, layers, area, skin, action, pill_radius(area), false)?;
        ctx.draw_rounded_frame(
            layers,
            0,
            area.origin.x,
            area.origin.y,
            area.size.width,
            area.size.height,
            bg,
            border,
            pill_radius(area),
        )?;
        let text_w = self.width(ctx, font, label).min(area.size.width);
        self.text(
            ctx,
            layers,
            font,
            area.origin.x + ((area.size.width - text_w) / 2.0).max(0.0),
            area.origin.y + (area.size.height - Self::text_h(font)) / 2.0,
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
/// stay correct, except the theme tiles, which depict physical things and
/// look the same either way.
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
    /// The open language menu, a step off the page so it reads as above it.
    menu_bg: LinearRgba,
    /// Whether the page is dark, which lit tiles shade for.
    dark: bool,
    shadow: LinearRgba,
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
            menu_bg: mix(ground, palette.text, 0.05),
            dark: palette.is_dark(),
            shadow: LinearRgba::with_components(0.0, 0.0, 0.0, 0.35),
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

/// Returns the Space the window was left on.
fn finish_onboarding(tw: &mut TermWindow, prefs: OnboardingPrefs) -> String {
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
            tw.switch_space(space_id.clone(), &window);
        } else {
            window.invalidate();
        }
    }
    tw.close_content_view();
    space_id
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

    /// The curtain's next step while it is up. Otherwise this polls the
    /// running check behind the found rows until it answers, or until
    /// FOUND_WAIT: a source wedged past that leaves its row saying how many
    /// sessions there are, and the page goes back to being still.
    fn next_frame_time(&self) -> Option<Instant> {
        if self.intro.is_some() {
            return Some(Instant::now() + INTRO_FRAME);
        }
        let curtain_up = self
            .curtain
            .as_ref()
            .is_some_and(|curtain| !curtain.columns.is_empty());
        if curtain_up {
            return Some(Instant::now() + CURTAIN_FRAME);
        }
        (self.found.pending() && self.opened.elapsed() < FOUND_WAIT)
            .then(|| Instant::now() + Duration::from_millis(250))
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
        caption_font: &Rc<LoadedFont>,
        _cursor_on: bool,
    ) -> anyhow::Result<()> {
        let frame = self
            .intro
            .as_mut()
            .and_then(|intro| intro.frame(Instant::now()));
        if frame.is_none() {
            self.intro = None;
        }
        let page_under = match (self.intro.as_mut(), frame) {
            (Some(intro), Some(frame)) => frame.page_in.is_some() || intro.wants_warm_up(frame),
            _ => true,
        };
        if page_under {
            self.paint_impl(
                ctx,
                layers,
                area,
                palette,
                font,
                title_font,
                section_font,
                caption_font,
            )?;
        } else {
            // Nothing of the page is drawn yet, so nothing can be hit.
            self.widgets.clear();
        }
        match (&self.intro, frame) {
            (Some(intro), Some(frame)) => {
                intro.paint(ctx, layers, area, palette.window_bg, frame, page_under)
            }
            _ => Ok(()),
        }
    }

    fn on_mouse(&mut self, x: f32, y: f32, kind: WMEK) -> ContentViewResponse {
        if self.intro.is_some() {
            return ContentViewResponse::Ignored;
        }
        self.on_mouse_impl(x, y, kind)
    }

    fn on_key(&mut self, key: KeyCode, mods: KeyModifiers) -> ContentViewResponse {
        if self.intro.is_some() {
            return ContentViewResponse::Ignored;
        }
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

    fn view(found: found::Found) -> OnboardingView {
        OnboardingView::with_found("space-default".to_string(), "Default".to_string(), found)
    }

    #[test]
    fn target_space_uses_initial_space() {
        let view = view(found::Found::default());
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
        let order = focus_order(&found::Found::default());
        assert_eq!(order.len(), 1 + 3 + 2);
        assert!(order.contains(&OnboardingAction::LanguageMenu));
        assert!(order.contains(&OnboardingAction::Start));
        assert!(order.contains(&OnboardingAction::Skip));
        for mode in [
            NativeThemeMode::System,
            NativeThemeMode::Light,
            NativeThemeMode::Dark,
        ] {
            assert!(order.contains(&OnboardingAction::Appearance(mode)));
        }

        let found = found::Found {
            sessions: vec![found::FoundSessions::for_test("example", 2, Some(1))],
            wezterm_config: true,
            editors: None,
        };
        let order = focus_order(&found);
        assert!(order.contains(&OnboardingAction::Import(Some("example"))));
        assert!(order.contains(&OnboardingAction::Import(None)));
        // Found rows come before the page's own buttons, as they are drawn.
        assert_eq!(
            order[order.len() - 2..],
            [OnboardingAction::Skip, OnboardingAction::Start]
        );
    }

    /// Nothing of the page shows during the intro, so neither the keys nor
    /// the pointer may reach it: Enter would start, Escape would skip.
    #[test]
    fn the_intro_takes_no_input() {
        let mut view = OnboardingView {
            intro: Some(hello::Intro::new()),
            ..view(found::Found::default())
        };
        for key in [
            KeyCode::Enter,
            KeyCode::Escape,
            KeyCode::Tab,
            KeyCode::Char(' '),
        ] {
            assert!(matches!(
                view.on_key(key, KeyModifiers::NONE),
                ContentViewResponse::Ignored
            ));
        }
        assert!(matches!(
            view.on_mouse(10.0, 10.0, WMEK::Press(MousePress::Left)),
            ContentViewResponse::Ignored
        ));

        view.intro = None;
        assert!(matches!(
            view.on_key(KeyCode::Escape, KeyModifiers::NONE),
            ContentViewResponse::Run(_)
        ));
    }

    /// The language menu takes the arrow keys and Escape while it is open, and
    /// gives Escape back to the page once it is shut.
    #[test]
    fn the_language_menu_keeps_its_keys_while_open() {
        let mut view = view(found::Found::default());
        view.interaction.focused = Some(OnboardingAction::LanguageMenu);
        view.on_key_impl(KeyCode::Enter, KeyModifiers::NONE);
        let opened = view.menu.expect("Enter on the pill opens the menu");
        assert_eq!(opened, language_index(view.selected_language));

        view.on_key_impl(KeyCode::DownArrow, KeyModifiers::NONE);
        assert_eq!(view.menu, Some((opened + 1) % LANGUAGE_OPTIONS.len()));
        view.on_key_impl(KeyCode::UpArrow, KeyModifiers::NONE);
        view.on_key_impl(KeyCode::UpArrow, KeyModifiers::NONE);
        assert_eq!(
            view.menu,
            Some((opened + LANGUAGE_OPTIONS.len() - 1) % LANGUAGE_OPTIONS.len())
        );

        assert!(matches!(
            view.on_key_impl(KeyCode::Escape, KeyModifiers::NONE),
            ContentViewResponse::Redraw
        ));
        assert_eq!(view.menu, None);
        // Shut, Escape is Skip again.
        assert!(matches!(
            view.on_key_impl(KeyCode::Escape, KeyModifiers::NONE),
            ContentViewResponse::Run(_)
        ));
    }

    /// The curtain is drawn in whatever the caption font is, on every
    /// platform; a character outside ASCII would show as a missing glyph
    /// wherever that font lacks it.
    #[test]
    fn the_curtain_keeps_to_characters_every_font_has() {
        assert!(CURTAIN_CHARS.is_ascii());
        assert!(CURTAIN_CHARS.iter().all(|c| c.is_ascii_graphic()));
        // A fixed pattern: the same place gives the same strand every frame.
        assert_eq!(curtain_hash(1, 7), curtain_hash(1, 7));
        assert_ne!(curtain_hash(0, 7), curtain_hash(1, 7));
    }

    /// Nothing here clips, so a character past a margin would paint over
    /// the sidebar on the left or the page in the middle -- at any moment.
    #[test]
    fn the_curtain_stays_inside_the_side_margins() {
        let area = rect(400.0, 60.0, 2000.0, 1200.0);
        let (col_w, clear, char_w, line_h) = (880.0, 40.0, 14.0, 30.0);
        let col_x = area.origin.x + (area.size.width - col_w) / 2.0;
        let black = LinearRgba::with_components(0.0, 0.0, 0.0, 1.0);
        let white = LinearRgba::with_components(1.0, 1.0, 1.0, 1.0);
        let lay_out = || Curtain::lay_out(area, col_x, col_w, clear, char_w, line_h, black, white);
        let mut curtain = lay_out();
        for elapsed in [0.0, 1.7, 30.0, 600.5] {
            curtain.frame(elapsed);
            assert!(curtain.glyphs.len() > 100, "a wide window hangs a curtain");
            for (_, x, y, _) in &curtain.glyphs {
                let left = *x >= area.origin.x && x + char_w <= col_x - clear;
                let right = *x >= col_x + col_w + clear && x + char_w <= area.max_x();
                assert!(left || right, "a character at x={x} left the margins");
                assert!(*y >= area.origin.y && y + line_h <= area.max_y());
            }
        }
        // The same moment of the same page is the same picture, and the
        // strands do move.
        curtain.frame(3.0);
        let mut again = lay_out();
        again.frame(3.0);
        assert_eq!(curtain.glyphs, again.glyphs);
        again.frame(3.5);
        assert_ne!(curtain.glyphs, again.glyphs);
        // A margin too narrow for two columns hangs nothing, so nothing
        // keeps the page redrawing.
        let narrow = rect(0.0, 0.0, col_w + 120.0, 600.0);
        let mut none = Curtain::lay_out(narrow, 60.0, col_w, clear, char_w, line_h, black, white);
        none.frame(1.0);
        assert!(none.columns.is_empty() && none.glyphs.is_empty());
    }

    #[test]
    fn a_session_count_waits_for_the_running_check() {
        let found = found::Found {
            sessions: vec![found::FoundSessions::for_test("example", 2, None)],
            wezterm_config: false,
            editors: None,
        };
        assert!(found.pending());
        assert!(!found.is_empty());
        let found = found::Found {
            sessions: vec![found::FoundSessions::for_test("example", 2, Some(0))],
            wezterm_config: false,
            editors: None,
        };
        assert!(!found.pending());
        assert!(found::Found::default().is_empty());
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
        assert_eq!(MARK_SIZE / 2.0, 72.0, "mark should read as 72pt");
        assert_eq!(COL_W / 2.0, 440.0, "column should read as 440pt");
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
                ("menu_bg", skin.menu_bg),
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
            let to_ground = (luma(skin.menu_bg) - luma(skin.ground)).abs();
            let to_text = (luma(skin.menu_bg) - luma(skin.text)).abs();
            assert!(
                to_ground < to_text,
                "menu on {:?} sits closer to the text than to the page",
                appearance
            );
        }
    }
}
