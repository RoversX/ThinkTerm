use super::font_presets::{font_presets, with_set, CjkLanguage, FontPreset};
use super::*;
use crate::native_settings::{FontStack, MAX_FONT_FALLBACKS};

const PAGE_SIZE: usize = 5;

/// What a font list is shown with: Latin, Chinese, Japanese and German,
/// each part drawn by the first font of the list that has it.
const FONT_SAMPLE: [&str; 4] = ["Hello", "你好", "こんにちは", "Grüße"];

/// How many fonts' sample results are kept: the list under a row and an
/// open picker's preview, with room for the ones they replace.
const SAMPLE_FACES_KEPT: usize = 4;
/// Family names that stand for a kind of font rather than one font.
const GENERIC_FAMILIES: [&str; 13] = [
    "monospace",
    "mono",
    "sans-serif",
    "sans",
    "serif",
    "system-ui",
    "ui-monospace",
    "ui-sans-serif",
    "ui-serif",
    "ui-rounded",
    "cursive",
    "fantasy",
    "emoji",
];

#[derive(Debug, Clone, Default)]
pub(super) struct FontCatalog {
    names: Option<Vec<String>>,
    generation: u64,
    loading: bool,
}

impl FontCatalog {
    pub(super) fn invalidate(&mut self) {
        self.names = None;
        self.generation = self.generation.wrapping_add(1);
    }

    fn begin(&mut self) -> Option<u64> {
        if self.loading || self.names.is_some() {
            return None;
        }
        self.loading = true;
        Some(self.generation)
    }

    fn finish(
        &mut self,
        generation: u64,
        result: anyhow::Result<Vec<String>>,
    ) -> anyhow::Result<bool> {
        self.loading = false;
        if generation != self.generation {
            return Ok(false);
        }
        let mut names = result?;
        // A row names its family by a u16 index (FontPickerChoose); any
        // family past that range would select the wrong one.
        names.truncate(usize::from(u16::MAX) + 1);
        self.names = Some(names);
        Ok(true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FontTarget {
    Interface,
    Terminal,
}

/// Which place in a font list the picker chooses for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FontSlot {
    /// The main font, which may be the default.
    Main,
    /// A new fallback font, after the others.
    Add,
    /// The fallback font at this place.
    Fallback(u8),
    /// All the fallback fonts at once, from a recommended set.
    Recommended,
}

/// What one loaded font is made of: the names of the faces it loaded, the
/// main one first, and, once the sample has been shaped in it, which
/// families draw each part of `FONT_SAMPLE`. Names only: the faces
/// themselves would keep their fonts' data alive after the font is gone.
#[derive(Debug)]
pub(super) struct SampleFaces {
    font: wezterm_font::LoadedFontId,
    faces: Vec<FaceNames>,
    /// For each part, the families that draw it in the order they come;
    /// empty where no font has it. `None` until the sample is shaped.
    parts: Option<Vec<Vec<String>>>,
}

/// The names a loaded face answers to.
#[derive(Debug)]
struct FaceNames {
    names: wezterm_font::parser::Names,
    path: Option<String>,
}

impl FaceNames {
    fn of(face: &wezterm_font::parser::ParsedFont) -> Self {
        Self {
            names: face.names().clone(),
            path: face.handle.path_str().map(String::from),
        }
    }

    /// Whether the loader takes `family` for this face, as
    /// `ParsedFont::matches_name` does.
    fn matches(&self, family: &str) -> bool {
        let names = &self.names;
        family == names.family
            || self.path.as_deref() == Some(family)
            || family == names.full_name
            || names.postscript_name.as_deref() == Some(family)
            || names.aliases.iter().any(|alias| alias == family)
    }
}

impl SampleFaces {
    /// With `shaped`, the sample is shaped as the window draws text,
    /// waiting for the fallback fonts: what this reports is what is shown.
    /// Without, nothing is shaped, so nothing is looked up either.
    fn of(font: &LoadedFont, shaped: bool) -> Self {
        let drawn_by: Option<Vec<Vec<usize>>> = shaped.then(|| {
            FONT_SAMPLE
                .iter()
                .map(|part| faces_drawing(font, part))
                .collect()
        });
        // After shaping, which may have added the system's fallbacks.
        let faces: Vec<FaceNames> = font.clone_handles().iter().map(FaceNames::of).collect();
        let parts = drawn_by.map(|parts| {
            parts
                .into_iter()
                .map(|indices| {
                    indices
                        .into_iter()
                        .filter_map(|index| faces.get(index))
                        .map(|face| face.names.family.clone())
                        .collect()
                })
                .collect()
        });
        Self {
            font: font.id(),
            faces,
            parts,
        }
    }

    /// Whether `family` is one of the fonts loaded, by the names the loader
    /// takes: a family listed but not installed here is not, nor is one
    /// written so that the loader passes it by.
    fn has(&self, family: &str) -> bool {
        self.faces.iter().any(|face| face.matches(family))
    }

    /// The family standing in for `primary` when the main font loaded is
    /// not it: the list's next font, or whatever the system offered in its
    /// place. `None` when it is.
    fn stand_in_for(&self, primary: &str) -> Option<String> {
        // A generic family is the system's to resolve to a face.
        if GENERIC_FAMILIES
            .iter()
            .any(|generic| generic.eq_ignore_ascii_case(primary.trim()))
        {
            return None;
        }
        let main = self.faces.first()?;
        let names = &main.names;
        // Fontconfig and Core Text also take a family in another case.
        let answers = main.matches(primary)
            || std::iter::once(&names.family)
                .chain(std::iter::once(&names.full_name))
                .chain(names.postscript_name.iter())
                .chain(names.aliases.iter())
                .any(|name| name.eq_ignore_ascii_case(primary));
        (!answers).then(|| display_family(&names.family))
    }
}

/// The places in `font`'s faces of the ones that draw `text`, each once,
/// in the order they first come; none for characters no face has.
fn faces_drawing(font: &LoadedFont, text: &str) -> Vec<usize> {
    let mut faces = Vec::new();
    for glyph in font
        .blocking_shape(text, None, Direction::LeftToRight, None, None)
        .unwrap_or_default()
    {
        if glyph.glyph_pos != 0 && !glyph.is_space && !faces.contains(&glyph.font_idx) {
            faces.push(glyph.font_idx);
        }
    }
    faces
}

/// A family as the Settings window names it: the system's own interface
/// face, hidden under a name starting with a dot, as "System font".
fn display_family(family: &str) -> String {
    if family.starts_with('.') {
        crate::i18n::tr("settings-font-system")
    } else {
        family.to_string()
    }
}

/// What a picker's choice changes in a font list, so that it can be made
/// to the latest list rather than the one the picker opened with.
#[derive(Debug, Clone)]
struct FontEdit {
    slot: FontSlot,
    draft: Option<String>,
    /// For a fallback's place, the family the picker opened on.
    replacing: Option<String>,
    /// For the recommended sets, the fallbacks of the one chosen.
    set: Option<Vec<String>>,
}

impl FontEdit {
    fn apply(&self, stack: &mut FontStack) {
        match self.slot {
            FontSlot::Main => stack.primary.clone_from(&self.draft),
            FontSlot::Add => stack.fallbacks.extend(self.draft.clone()),
            // Ahead of the list's other fonts, whatever it has now: the
            // main font stays, and so do symbols and emoji fonts.
            FontSlot::Recommended => {
                if let Some(set) = &self.set {
                    stack.fallbacks = with_set(set, &stack.fallbacks);
                }
            }
            FontSlot::Fallback(_) => {
                let Some(draft) = &self.draft else {
                    return;
                };
                // Found by its name: the list may have moved since. One
                // taken out meanwhile leaves no place, so the choice goes
                // after the others rather than nowhere.
                match self.replacing.as_ref().and_then(|replacing| {
                    stack
                        .fallbacks
                        .iter_mut()
                        .find(|family| family.eq_ignore_ascii_case(replacing))
                }) {
                    Some(place) => place.clone_from(draft),
                    None => stack.fallbacks.push(draft.clone()),
                }
            }
        }
    }

    /// The family this edit puts in the list, which must be there after.
    fn adds(&self) -> Option<&str> {
        match self.slot {
            FontSlot::Main | FontSlot::Recommended => None,
            FontSlot::Add | FontSlot::Fallback(_) => self.draft.as_deref(),
        }
    }
}

/// Move `family` one place up (`true`) or down in `stack`'s fallbacks,
/// found by name: the list may have moved since it was painted.
fn move_fallback(stack: &mut FontStack, family: &str, up: bool) {
    let Some(at) = stack
        .fallbacks
        .iter()
        .position(|listed| listed.eq_ignore_ascii_case(family))
    else {
        return;
    };
    let other = if up {
        at.checked_sub(1)
    } else {
        at.checked_add(1)
    };
    if let Some(other) = other.filter(|&other| other < stack.fallbacks.len()) {
        stack.fallbacks.swap(at, other);
    }
}

/// Take `family` out of `stack`'s fallbacks, found by name.
fn remove_fallback(stack: &mut FontStack, family: &str) {
    stack
        .fallbacks
        .retain(|listed| !listed.eq_ignore_ascii_case(family));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreviewKey {
    dpi: usize,
    generation: usize,
    system_fallback: bool,
}

impl PreviewKey {
    fn new(fonts: &FontConfiguration, config: &config::ConfigHandle) -> Self {
        Self {
            dpi: fonts.get_dpi(),
            generation: config.generation(),
            system_fallback: fonts.config().fontconfig_prefer_system_fallback,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct FontPicker {
    pub target: FontTarget,
    pub slot: FontSlot,
    pub search: TextInputState,
    /// The family picked for `slot`; `None` is the default for the main
    /// font and nothing picked yet for a fallback.
    pub draft: Option<String>,
    pub page: usize,
    /// The list as it was when the picker opened: what its preview and
    /// offers go by. Saving changes only the slot, in the latest list.
    base: FontStack,
    /// The main font when `base` leaves it to the configuration: a
    /// fallback picked is never that, which the list would pass by.
    default_main: Option<String>,
    /// The configuration generation `default_main` is from.
    default_main_generation: Option<usize>,
    /// For a fallback's place, the family it opened on.
    replacing: Option<String>,
    /// For the recommended sets, the ones installed here, and the
    /// catalogue they are from: its generation and length.
    presets: Vec<FontPreset>,
    presets_from: Option<(u64, usize)>,
    /// The set shown and applied.
    preset: Option<FontPreset>,
    /// The language of the set chosen by hand; until then `preset` is the
    /// first set, which a new catalogue may change.
    chosen: Option<CjkLanguage>,
    preview: Option<Rc<LoadedFont>>,
    preview_key: Option<PreviewKey>,
    filtered: Vec<usize>,
    filter_query: Option<String>,
    catalog_len: usize,
}

impl FontPicker {
    fn new(
        target: FontTarget,
        slot: FontSlot,
        base: FontStack,
        default_main: Option<String>,
    ) -> Self {
        let replacing = match slot {
            FontSlot::Fallback(index) => base.fallbacks.get(usize::from(index)).cloned(),
            FontSlot::Main | FontSlot::Add | FontSlot::Recommended => None,
        };
        let draft = match slot {
            FontSlot::Main => base.primary.clone(),
            FontSlot::Add | FontSlot::Recommended => None,
            FontSlot::Fallback(_) => replacing.clone(),
        };
        Self {
            target,
            slot,
            search: TextInputState::new(),
            draft,
            page: 0,
            base,
            default_main,
            default_main_generation: None,
            replacing,
            presets: Vec::new(),
            presets_from: None,
            preset: None,
            chosen: None,
            preview: None,
            preview_key: None,
            filtered: Vec::new(),
            filter_query: None,
            catalog_len: 0,
        }
    }

    pub(super) fn clear_preview(&mut self) {
        self.preview = None;
        self.preview_key = None;
    }

    fn edit(&self) -> FontEdit {
        FontEdit {
            slot: self.slot,
            draft: self.draft.clone(),
            replacing: self.replacing.clone(),
            set: self.preset.as_ref().map(|preset| preset.families.clone()),
        }
    }

    /// The list the picker opened with, with the draft in its place. A
    /// main font picked from the fallbacks leaves them; `FontStack::new`
    /// drops the repeat.
    fn draft_stack(&self) -> FontStack {
        let mut stack = self.base.clone();
        self.edit().apply(&mut stack);
        FontStack::new(stack.primary.as_deref(), &stack.fallbacks)
    }

    /// The families not to offer: a fallback picked is one the list does
    /// not have yet, nor the main font. Any family may become the main font.
    fn excluded(&self) -> Vec<String> {
        let keep = match self.slot {
            FontSlot::Main | FontSlot::Recommended => return Vec::new(),
            FontSlot::Add => None,
            FontSlot::Fallback(index) => Some(usize::from(index)),
        };
        self.base
            .primary
            .as_ref()
            .or(self.default_main.as_ref())
            .into_iter()
            .chain(
                self.base
                    .fallbacks
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| Some(*index) != keep)
                    .map(|(_, family)| family),
            )
            .map(|family| family.to_lowercase())
            .collect()
    }

    /// Whether Apply has something to save.
    fn can_apply(&self) -> bool {
        self.preview.is_some()
            && match self.slot {
                FontSlot::Main => true,
                FontSlot::Recommended => self.preset.is_some(),
                FontSlot::Add | FontSlot::Fallback(_) => self.draft.is_some(),
            }
    }

    /// The recommended sets for the catalogue as `names` (`None` while it
    /// loads) and `generation` have it, worked out again when it changes.
    /// The set shown is the one chosen, with the fonts installed now, and
    /// until one is, the first: the system's language's.
    fn refresh_presets(&mut self, names: Option<&[String]>, generation: u64) {
        if self.slot != FontSlot::Recommended {
            return;
        }
        let Some(names) = names else {
            // Loading again: fonts listed may have gone, so there is
            // nothing to show or apply until the new list is in.
            if self.presets_from.take().is_some() {
                self.presets.clear();
                if self.preset.take().is_some() {
                    self.clear_preview();
                }
            }
            return;
        };
        let from = (generation, names.len());
        if self.presets_from == Some(from) {
            return;
        }
        // The system's languages first: the interface's has only the
        // languages there are translations for, Simplified Chinese among
        // them but not Traditional.
        let locales: Vec<String> = crate::i18n::system_locales()
            .into_iter()
            .chain(std::iter::once(crate::i18n::current_locale().to_string()))
            .collect();
        self.presets = font_presets(self.target, names, &locales);
        self.presets_from = Some(from);
        let shown = self
            .chosen
            .and_then(|language| {
                self.presets
                    .iter()
                    .find(|preset| preset.language == language)
            })
            .or(self.presets.first())
            .cloned();
        if shown != self.preset {
            self.preset = shown;
            self.clear_preview();
        }
    }

    fn load_preview(&mut self, fonts: &FontConfiguration, config: &config::ConfigHandle) {
        let key = PreviewKey::new(fonts, config);
        if self.preview_key == Some(key) {
            return;
        }
        let stack = self.draft_stack();
        let loaded = match self.target {
            FontTarget::Interface => {
                fonts.ui_font_preview(stack.primary.as_deref(), &stack.fallbacks, 14.0)
            }
            FontTarget::Terminal => {
                let config = config.adjusted(|adjusted| {
                    adjusted.font = crate::native_settings::terminal_font_style(config, &stack);
                });
                fonts.terminal_font_uncached(&config, 14.0)
            }
        };
        let loaded = loaded.and_then(|font| {
            if let Some(family) = &self.draft {
                let requested = config::FontAttributes::new(family);
                anyhow::ensure!(
                    font.clone_handles()
                        .iter()
                        .any(|font| font.matches_name(&requested)),
                    "font unavailable"
                );
            }
            Ok(font)
        });
        self.preview_key = Some(key);
        self.preview = loaded.ok();
    }
}

/// The catalogue's families containing `query`, by their place in it,
/// without the `excluded` ones (lowercase). `prefer_mono` keeps the
/// familiar terminal families near the top.
fn ranked_indices(
    names: &[String],
    query: &str,
    prefer_mono: bool,
    excluded: &[String],
) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    let mut found: Vec<usize> = names
        .iter()
        .enumerate()
        .filter(|(_, name)| {
            let name = name.to_lowercase();
            name.contains(&query) && !excluded.contains(&name)
        })
        .map(|(index, _)| index)
        .collect();
    if prefer_mono {
        // A suggestion, not a font filter: other installed choices stay.
        found.sort_by_cached_key(|&index| {
            let name = names[index].to_lowercase();
            !(name.contains("mono")
                || matches!(
                    name.as_str(),
                    "menlo" | "consolas" | "courier new" | "cascadia code"
                ))
        });
    }
    found
}

/// Where a part of the font sample goes, from the sample's top left: its
/// chip's x, y and width, and the caption under the part.
struct SampleChip {
    x: f32,
    y: f32,
    width: f32,
    caption: String,
}

impl SettingsWindow {
    pub(super) fn open_font_picker(&mut self, target: FontTarget, slot: FontSlot) {
        let was_open = self
            .ui
            .font_picker
            .as_ref()
            .is_some_and(|picker| picker.target == target && picker.slot == slot);
        self.close_font_picker();
        let stack = self.font_stack(target);
        if was_open || (slot == FontSlot::Add && stack.fallbacks.len() >= MAX_FONT_FALLBACKS) {
            return;
        }
        let config = configuration();
        let default_main = stack
            .primary
            .is_none()
            .then(|| Self::default_main_family(target, &config))
            .flatten();
        let mut picker = FontPicker::new(target, slot, stack, default_main);
        picker.default_main_generation = Some(config.generation());
        self.ui.font_picker = Some(picker);
        // The sets have no search field to type in, nor does anything
        // under them: a field focused before would take the keys.
        self.set_focused_input(
            (slot != FontSlot::Recommended).then_some(SettingsAction::FontPickerSearch),
        );
        self.load_font_catalog();
    }

    pub(super) fn invalidate_font_catalog(&mut self) {
        self.ui.font_catalog.invalidate();
        if let Some(picker) = &mut self.ui.font_picker {
            picker.filter_query = None;
            picker.filtered.clear();
            self.load_font_catalog();
        }
    }

    fn load_font_catalog(&mut self) {
        let Some(generation) = self.ui.font_catalog.begin() else {
            return;
        };
        let job = self.fonts.font_family_catalog_job();
        let instance_id = self.instance_id;
        std::thread::spawn(move || {
            let result = job();
            promise::spawn::spawn_into_main_thread(async move {
                if let Some(settings) = settings_window_for_instance(instance_id) {
                    let mut settings = settings.borrow_mut();
                    match settings.ui.font_catalog.finish(generation, result) {
                        Ok(true) => {}
                        Ok(false) => {
                            if settings.ui.font_picker.is_some() {
                                settings.load_font_catalog();
                            }
                            return;
                        }
                        Err(error) => {
                            log::warn!("font family catalogue: {error:#}");
                            settings.status = crate::i18n::tr("settings-font-list-error");
                        }
                    }
                    if let Some(window) = settings.window.as_ref() {
                        window.invalidate();
                    }
                }
            })
            .detach();
        });
    }

    pub(super) fn close_font_picker(&mut self) {
        if self.ui.font_picker.take().is_some() {
            self.invalidate_shaped_text();
            self.ui.stale_glyphs = true;
        }
        if self.ui.interaction.focused == Some(SettingsAction::FontPickerSearch) {
            self.set_focused_input(None);
        }
    }

    pub(super) fn choose_font(&mut self, index: Option<(u16, u64)>) {
        let family = match index {
            None => None,
            Some((index, generation)) => {
                if generation != self.ui.font_catalog.generation {
                    return;
                }
                let Some(name) = self
                    .ui
                    .font_catalog
                    .names
                    .as_ref()
                    .and_then(|names| names.get(index as usize))
                else {
                    return;
                };
                Some(name.clone())
            }
        };
        if let Some(picker) = &mut self.ui.font_picker {
            picker.draft = family;
            picker.clear_preview();
        }
        self.invalidate_shaped_text();
        self.ui.stale_glyphs = true;
    }

    /// Choose the recommended set for `language`, as the sets are now.
    pub(super) fn choose_font_preset(&mut self, language: CjkLanguage) {
        let Some(picker) = &mut self.ui.font_picker else {
            return;
        };
        let Some(preset) = picker
            .presets
            .iter()
            .find(|preset| preset.language == language)
            .cloned()
        else {
            return;
        };
        picker.chosen = Some(language);
        if picker.preset.as_ref() != Some(&preset) {
            picker.preset = Some(preset);
            picker.clear_preview();
        }
        self.invalidate_shaped_text();
        self.ui.stale_glyphs = true;
    }

    pub(super) fn apply_font_picker(&mut self) {
        let Some(picker) = &self.ui.font_picker else {
            return;
        };
        if picker.preview_key != Some(PreviewKey::new(&self.fonts, &configuration()))
            || !self.font_picker_can_apply()
        {
            return;
        }
        let target = picker.target;
        let edit = picker.edit();
        let adds = edit.adds().map(String::from);
        self.save_font_stack(target, |stack| edit.apply(stack), adds.as_deref());
    }

    /// Whether the open picker's Apply has something to save to the list
    /// as this window has it now: a family added must find room there.
    fn font_picker_can_apply(&self) -> bool {
        let Some(picker) = &self.ui.font_picker else {
            return false;
        };
        if !picker.can_apply() {
            return false;
        }
        let edit = picker.edit();
        let Some(adds) = edit.adds() else {
            return true;
        };
        let mut stack = self.font_stack(picker.target);
        edit.apply(&mut stack);
        FontStack::new(stack.primary.as_deref(), &stack.fallbacks).has_fallback(adds)
    }

    /// The family `target`'s main font is when Settings leaves it to
    /// `config`; `None` where the interface has several.
    fn default_main_family(target: FontTarget, config: &config::ConfigHandle) -> Option<String> {
        match target {
            FontTarget::Interface => wezterm_font::default_ui_font_family(config),
            FontTarget::Terminal => Some(
                config
                    .font
                    .font
                    .first()
                    .map(|attr| attr.family.clone())
                    .unwrap_or_else(|| config::FontAttributes::default().family),
            ),
        }
    }

    /// The fallback at `index` in the list as painted, moved by name.
    pub(super) fn move_font_fallback(&mut self, target: FontTarget, index: usize, up: bool) {
        if let Some(family) = self.font_stack(target).fallbacks.get(index).cloned() {
            self.save_font_stack(target, |stack| move_fallback(stack, &family, up), None);
        }
    }

    /// The fallback at `index` in the list as painted, removed by name.
    pub(super) fn remove_font_fallback(&mut self, target: FontTarget, index: usize) {
        if let Some(family) = self.font_stack(target).fallbacks.get(index).cloned() {
            self.save_font_stack(target, |stack| remove_fallback(stack, &family), None);
        }
    }

    /// The font list `target` has in this window's copy of the settings.
    fn font_stack(&self, target: FontTarget) -> FontStack {
        match target {
            FontTarget::Interface => crate::native_settings::ui_font_stack(&self.native_settings),
            FontTarget::Terminal => {
                crate::native_settings::terminal_font_stack(&self.native_settings)
            }
        }
    }

    /// Change `target`'s font list with `edit` and save it, then show it
    /// everywhere. Made to the latest settings, as every change is (see
    /// `save_and_apply_native_terminal_settings`), so `edit` must work on
    /// whatever list it is handed. Once saved an open picker closes: it was
    /// choosing for the list as it was. One that could not save stays, so
    /// the choice can be tried again, and so does one whose choice, `adds`,
    /// found no place among the fallbacks meanwhile.
    fn save_font_stack(
        &mut self,
        target: FontTarget,
        edit: impl FnOnce(&mut FontStack),
        adds: Option<&str>,
    ) {
        let mut changed = false;
        let mut fits = true;
        let store = |settings: &mut ThinkTermNativeSettings| {
            let before = match target {
                FontTarget::Interface => crate::native_settings::ui_font_stack(settings),
                FontTarget::Terminal => crate::native_settings::terminal_font_stack(settings),
            };
            let mut stack = before.clone();
            edit(&mut stack);
            let stack = FontStack::new(stack.primary.as_deref(), &stack.fallbacks);
            // A family to add that found no place among the fallbacks (the
            // list full, or the family now its main font) leaves the list
            // as it was.
            fits = adds.is_none_or(|family| stack.has_fallback(family));
            if !fits {
                return;
            }
            changed = stack != before;
            match target {
                FontTarget::Interface => {
                    settings.chrome.ui_font_family = stack.primary;
                    settings.chrome.ui_font_fallbacks = stack.fallbacks;
                }
                FontTarget::Terminal => {
                    settings.terminal.font_family = stack.primary;
                    settings.terminal.font_fallbacks = stack.fallbacks;
                }
            }
        };
        // The interface font is no setting a configuration reload reads.
        let result = match target {
            FontTarget::Interface => crate::native_settings::update_ui_state(store),
            FontTarget::Terminal => crate::native_settings::update(store).map(drop),
        };
        match result {
            Ok(()) if !fits => {
                self.status = settings_tr(
                    "settings-font-fallbacks-full",
                    &[("count", MAX_FONT_FALLBACKS.to_string())],
                );
            }
            Ok(()) if !changed => self.close_font_picker(),
            Ok(()) => {
                self.close_font_picker();
                let saved = crate::native_settings::load();
                match target {
                    FontTarget::Interface => crate::native_settings::apply_ui_font_to_app(&saved),
                    FontTarget::Terminal => crate::native_settings::apply_to_app_with_font_changes(
                        &saved,
                        crate::native_settings::FontChanges {
                            family: true,
                            ..Default::default()
                        },
                    ),
                }
                self.set_native_settings(saved);
                self.status = crate::i18n::tr("settings-font-applied");
            }
            Err(error) => {
                log::warn!("saving the font list: {error:#}");
                self.status = settings_tr(
                    "settings-status-terminal-error",
                    &[("error", format!("{error:#}"))],
                );
            }
        }
    }

    /// `font`'s `SampleFaces`, worked out once for it; `shaped` when the
    /// sample is to be shown.
    fn sample_faces(&mut self, font: &Rc<LoadedFont>, shaped: bool) -> Rc<SampleFaces> {
        let kept = &mut self.ui.font_sample_faces;
        if let Some(index) = kept.iter().position(|faces| faces.font == font.id()) {
            if !shaped || kept[index].parts.is_some() {
                return Rc::clone(&kept[index]);
            }
            kept.remove(index);
        }
        let faces = Rc::new(SampleFaces::of(font, shaped));
        if kept.len() >= SAMPLE_FACES_KEPT {
            kept.remove(0);
        }
        kept.push(Rc::clone(&faces));
        faces
    }

    /// The font the list under `target`'s row shows its sample in: the one
    /// the page already has for it.
    fn font_sample_font(&mut self, target: FontTarget) -> Rc<LoadedFont> {
        match target {
            FontTarget::Interface => Rc::clone(&self.ui_font),
            FontTarget::Terminal => {
                let size = self.current_terminal_font_size_value();
                self.preview_font(PreviewFont::Terminal, size)
            }
        }
    }

    /// The fonts the configuration file lists after its main font, which
    /// come after `stack`'s: the terminal's only, as the interface goes by
    /// the terminal's after its own.
    fn configured_font_extras(&self, target: FontTarget, stack: &FontStack) -> Vec<String> {
        if target != FontTarget::Terminal {
            return Vec::new();
        }
        let config = configuration();
        let mut extras: Vec<String> = Vec::new();
        for attr in config.font.font.iter().skip(1) {
            let family = attr.family.trim();
            let listed = |other: &String| other.eq_ignore_ascii_case(family);
            if family.is_empty()
                || stack.primary.iter().any(listed)
                || stack.fallbacks.iter().any(listed)
                || extras.iter().any(listed)
            {
                continue;
            }
            extras.push(family.to_string());
        }
        extras
    }

    pub(super) fn paint_font_choice(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        target: FontTarget,
        rule: bool,
    ) -> anyhow::Result<f32> {
        let title = crate::i18n::tr(match target {
            FontTarget::Interface => "settings-interface-font",
            FontTarget::Terminal => "settings-terminal-font-family",
        });
        let description = crate::i18n::tr(match target {
            FontTarget::Interface => "settings-interface-font-description",
            FontTarget::Terminal => "settings-font-choice-description",
        });
        let current = match target {
            FontTarget::Interface => self.native_settings.chrome.ui_font_family.as_deref(),
            FontTarget::Terminal => self.native_settings.terminal.font_family.as_deref(),
        }
        .map(str::to_string)
        .unwrap_or_else(|| crate::i18n::tr("settings-font-default"));
        if rule {
            self.paint_separator(layers, x, y - self.ui_px(28.0), width)?;
        }
        let (cx, cy, cw) = self.dropdown_control_geometry(x, y, width);
        let control = self.dropdown_pill_rect(cx, cy, cw, &current);
        let action = SettingsAction::OpenFontPicker(target);
        let palette = self.palette();
        let open = self
            .ui
            .font_picker
            .as_ref()
            .is_some_and(|picker| picker.target == target && picker.slot == FontSlot::Main);
        let hovered = self.ui.interaction.hovered == Some(action);
        self.ui_context.push(control, WidgetKind::Button, action);
        self.paint_dropdown_pill(
            layers,
            control,
            &current,
            if hovered {
                palette.control_hover_bg
            } else {
                palette.control_bg
            },
            if open {
                palette.nav_selected_bg
            } else {
                palette.control_border
            },
        )?;
        let font = Rc::clone(&self.ui_font);
        let text_width = (cx - x - self.ui_px(24.0)).max(width * 0.45);
        self.draw_text(layers, &font, x, y, &title, palette.text, text_width)?;
        let extra = self.draw_row_description(layers, x, y, &description, text_width)?;
        let row_bottom = y + self.settings_row_visual_height();
        let mut bottom = self.paint_font_fallbacks(
            layers,
            x,
            row_bottom + extra + self.ui_px(20.0),
            width,
            target,
        )?;
        if self
            .ui
            .font_picker
            .as_ref()
            .is_some_and(|picker| picker.target == target)
        {
            bottom = self.paint_font_picker(layers, x, bottom + self.ui_px(24.0), width)?
                + self.ui_px(8.0);
        } else {
            bottom += self.ui_px(28.0);
        }
        Ok(bottom - row_bottom)
    }

    /// The list under a font row: its fallback fonts in order, each to
    /// move, remove or replace, a row to add one, a word on the order, and
    /// the sample in the fonts the list makes, with the picker's choice
    /// while it is open, each part over the family that draws it. Returns
    /// where it ends.
    fn paint_font_fallbacks(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        top: f32,
        width: f32,
        target: FontTarget,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let accent = self.chrome_palette.accent;
        let fill = self.chrome_palette.group_bg;
        let stack = self.font_stack(target);
        let count = stack.fallbacks.len();
        // While the picker is open on this list, the sample shows the list
        // with its choice in it, and none for a choice that cannot load.
        let picking = self.font_picker_preview(target)?;
        let picked = picking.clone().flatten();
        // The sample only with a list or a choice to show it for: shaping
        // it looks up a font for its Chinese and Japanese, which a page
        // someone only scrolls past should not wait on.
        let shaped = match &picking {
            Some(picked) => picked.is_some(),
            None => count > 0,
        };
        // The rows and notes are about the list as saved.
        let saved_font = self.font_sample_font(target);
        let faces = self.sample_faces(&saved_font, count > 0 && picking.is_none());
        let (sample_font, sample_faces) = match picked {
            Some(font) => {
                let faces = self.sample_faces(&font, true);
                (font, faces)
            }
            None => (saved_font, Rc::clone(&faces)),
        };
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        let editing = self
            .ui
            .font_picker
            .as_ref()
            .filter(|picker| picker.target == target)
            .map(|picker| picker.slot);
        let row_height = self.dropdown_row_height();
        let pad_y = self.ui_px(6.0);
        let lead = self.ui_px(18.0);
        let badge = self.ui_px(36.0);
        let label_x = x + lead + badge + self.ui_px(18.0);
        let button = (row_height - self.ui_px(14.0)).clamp(self.ui_px(40.0), self.ui_px(52.0));
        let buttons_x = x + width - self.ui_px(10.0) - button * 3.0;
        let mut y = top;

        if count > 0 {
            self.draw_text(
                layers,
                &body_font,
                x,
                y,
                &crate::i18n::tr("settings-font-fallbacks"),
                palette.secondary_text,
                width,
            )?;
            y += self.description_line_step() + self.ui_px(8.0);
        }
        let group_height = pad_y * 2.0 + row_height * (count + 1) as f32;
        self.draw_rounded_rect(
            layers,
            0,
            x,
            y,
            width,
            group_height,
            fill,
            self.ui_px(24.0).min(group_height / 2.0),
        )?;
        for (index, family) in stack.fallbacks.iter().enumerate() {
            let place = index.min(usize::from(u8::MAX)) as u8;
            let row_y = y + pad_y + row_height * index as f32;
            if index > 0 {
                self.paint_separator(layers, label_x, row_y, x + width - label_x - lead)?;
            }
            let row = rect(x, row_y, width, row_height);
            let action = SettingsAction::EditFontFallback(target, place);
            // Short of the buttons, so that a faint one takes no click.
            self.ui_context.push(
                rect(x, row_y, buttons_x - x, row_height),
                WidgetKind::Button,
                action,
            );
            self.paint_font_list_row_ground(
                layers,
                row,
                fill,
                action,
                editing == Some(FontSlot::Fallback(place)),
            )?;

            let badge_y = row_y + (row_height - badge) / 2.0;
            self.draw_rounded_rect(
                layers,
                0,
                x + lead,
                badge_y,
                badge,
                badge,
                crate::ui::color::mix(fill, palette.text, 0.12),
                badge / 2.0,
            )?;
            let number = (index + 1).to_string();
            let number_width = self.measure_text_width(&body_font, &number);
            self.draw_text(
                layers,
                &body_font,
                (x + lead + (badge - number_width) / 2.0).round(),
                self.control_text_y(badge_y, badge),
                &number,
                palette.secondary_text,
                badge,
            )?;

            let label_y = self.control_text_y(row_y, row_height);
            let room = (buttons_x - label_x - self.ui_px(12.0)).max(0.0);
            let missing = !faces.has(family);
            let tag = missing.then(|| crate::i18n::tr("settings-font-not-installed"));
            let tag_room = tag.as_ref().map_or(0.0, |tag| {
                self.measure_text_width(&body_font, tag) + self.ui_px(16.0)
            });
            let name = self.text_with_ellipsis(&ui_font, family, (room - tag_room).max(0.0));
            self.draw_text(
                layers,
                &ui_font,
                label_x,
                label_y,
                &name,
                if missing {
                    palette.secondary_text
                } else {
                    palette.text
                },
                room,
            )?;
            if let Some(tag) = tag {
                let tag_x = label_x + self.measure_text_width(&ui_font, &name) + self.ui_px(16.0);
                self.draw_text(
                    layers,
                    &body_font,
                    tag_x,
                    label_y,
                    &tag,
                    palette.muted_text,
                    (label_x + room - tag_x).max(0.0),
                )?;
            }

            let button_y = row_y + (row_height - button) / 2.0;
            let controls = [
                (
                    SvgIcon::ChevronUp,
                    (index > 0).then_some(SettingsAction::MoveFontFallback(target, place, true)),
                ),
                (
                    SvgIcon::ChevronDown,
                    (index + 1 < count)
                        .then_some(SettingsAction::MoveFontFallback(target, place, false)),
                ),
                (
                    SvgIcon::X,
                    Some(SettingsAction::RemoveFontFallback(target, place)),
                ),
            ];
            for (slot, &(icon, action)) in controls.iter().enumerate() {
                let button_x = buttons_x + button * slot as f32;
                match action {
                    Some(action) => {
                        self.paint_icon_button(layers, button_x, button_y, button, icon, action)?
                    }
                    None => {
                        // In its place but faint, so the others do not move.
                        let glyph = (self.metrics.cell_size.height as f32 + 4.0)
                            .clamp(self.ui_px(24.0), self.ui_px(34.0));
                        self.draw_svg_icon(
                            layers,
                            icon,
                            button_x + (button - glyph) / 2.0,
                            button_y + (button - glyph) / 2.0,
                            glyph,
                            palette.muted_text.mul_alpha(0.35),
                        )?;
                    }
                }
            }
        }

        let row_y = y + pad_y + row_height * count as f32;
        if count > 0 {
            self.paint_separator(layers, label_x, row_y, x + width - label_x - lead)?;
        }
        // The recommended sets, at the end of the row to add a font: the
        // list full or not, a set takes its place.
        let recommend_label = crate::i18n::tr("settings-font-recommended");
        let recommend_glyph = self.ui_px(26.0);
        let recommend_gap = self.ui_px(10.0);
        let recommend_width = (self.measure_text_width(&ui_font, &recommend_label)
            + recommend_glyph
            + recommend_gap
            + lead * 2.0)
            .min(width * 0.5);
        let recommend = rect(
            x + width - recommend_width,
            row_y,
            recommend_width,
            row_height,
        );
        let recommend_action = SettingsAction::RecommendFontFallbacks(target);
        self.ui_context
            .push(recommend, WidgetKind::Button, recommend_action);
        self.paint_font_list_row_ground(
            layers,
            recommend,
            fill,
            recommend_action,
            editing == Some(FontSlot::Recommended),
        )?;
        self.draw_svg_icon(
            layers,
            SvgIcon::Globe,
            recommend.min_x() + lead,
            row_y + (row_height - recommend_glyph) / 2.0,
            recommend_glyph,
            accent,
        )?;
        let recommend_text_x = recommend.min_x() + lead + recommend_glyph + recommend_gap;
        self.draw_text(
            layers,
            &ui_font,
            recommend_text_x,
            self.control_text_y(row_y, row_height),
            &recommend_label,
            accent,
            (recommend.max_x() - lead - recommend_text_x).max(0.0),
        )?;
        let (label, color) = if count >= MAX_FONT_FALLBACKS {
            (
                settings_tr(
                    "settings-font-fallbacks-full",
                    &[("count", MAX_FONT_FALLBACKS.to_string())],
                ),
                palette.muted_text,
            )
        } else {
            let row = rect(x, row_y, width - recommend_width, row_height);
            let action = SettingsAction::AddFontFallback(target);
            self.ui_context.push(row, WidgetKind::Button, action);
            self.paint_font_list_row_ground(
                layers,
                row,
                fill,
                action,
                editing == Some(FontSlot::Add),
            )?;
            (crate::i18n::tr("settings-font-fallbacks-add"), accent)
        };
        let glyph = self.ui_px(28.0);
        self.draw_svg_icon(
            layers,
            SvgIcon::Plus,
            x + lead + (badge - glyph) / 2.0,
            row_y + (row_height - glyph) / 2.0,
            glyph,
            color,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            label_x,
            self.control_text_y(row_y, row_height),
            &label,
            color,
            (recommend.min_x() - label_x - lead).max(0.0),
        )?;
        y += group_height;

        let extras = self.configured_font_extras(target, &stack);
        let mut notes = Vec::new();
        // Picked here, then uninstalled: another font draws everything,
        // the list's next or one the system put in its place.
        if let Some(primary) = &stack.primary {
            if let Some(used) = faces.stand_in_for(primary) {
                notes.push(settings_tr(
                    "settings-font-main-missing",
                    &[("font", primary.clone()), ("used", used)],
                ));
            }
        }
        if !extras.is_empty() {
            notes.push(settings_tr(
                "settings-font-fallbacks-config",
                &[("fonts", extras.join(", "))],
            ));
        }
        if count > 0 {
            notes.push(crate::i18n::tr("settings-font-fallbacks-note"));
        }
        for note in notes {
            y += self.ui_px(12.0);
            y += self.draw_row_note_in_lines(layers, x, y, &note, width, 2)?
                + self.description_line_step();
        }
        if !shaped {
            return Ok(y);
        }
        y += self.ui_px(18.0);
        self.paint_font_sample(layers, &sample_font, &sample_faces, x, y, width)
    }

    /// The picker's preview when it is open on `target`'s list: the list
    /// with the choice in it, `None` inside for a choice that cannot load.
    /// Loaded here for the sample over the picker, which is painted first.
    fn font_picker_preview(
        &mut self,
        target: FontTarget,
    ) -> anyhow::Result<Option<Option<Rc<LoadedFont>>>> {
        if !self
            .ui
            .font_picker
            .as_ref()
            .is_some_and(|picker| picker.target == target)
        {
            return Ok(None);
        }
        let config = configuration();
        if self.fonts.config().generation() != config.generation() {
            self.reload_configured_fonts(&config)?;
        }
        let Some(picker) = self.ui.font_picker.as_mut() else {
            return Ok(None);
        };
        // Before the preview, which shows the set chosen.
        picker.refresh_presets(
            self.ui.font_catalog.names.as_deref(),
            self.ui.font_catalog.generation,
        );
        picker.load_preview(&self.fonts, &config);
        Ok(Some(picker.preview.clone()))
    }

    /// A list row's ground: a step towards the text while it is under the
    /// pointer, and while the picker is choosing for it.
    fn paint_font_list_row_ground(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        row: window::RectF,
        fill: LinearRgba,
        action: SettingsAction,
        selected: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let step = if self.ui.interaction.pressed == Some(action) {
            0.12
        } else if selected {
            0.09
        } else if self.ui.interaction.hovered == Some(action) {
            0.06
        } else {
            return Ok(());
        };
        let inset_x = self.ui_px(DROPDOWN_HOVER_INSET_X);
        let inset_y = self.ui_px(DROPDOWN_HOVER_INSET_Y);
        let height = row.size.height - inset_y * 2.0;
        self.draw_rounded_rect(
            layers,
            0,
            row.origin.x + inset_x,
            row.origin.y + inset_y,
            row.size.width - inset_x * 2.0,
            height,
            crate::ui::color::mix(fill, palette.text, step),
            self.ui_px(DROPDOWN_ROW_RADIUS).min(height / 2.0),
        )
    }

    /// How tall one chip of the font sample in `font` is.
    fn font_sample_chip_height(&self, font: &LoadedFont) -> f32 {
        let part = RenderMetrics::with_font_metrics(&font.metrics())
            .cell_size
            .height as f32;
        self.ui_px(14.0) * 2.0 + part + self.ui_px(4.0) + self.metrics.cell_size.height as f32
    }

    /// Where the chips of the font sample in `font` go in `width`, as many
    /// to a line as fit, and how tall they are together.
    fn layout_font_sample(
        &self,
        font: &Rc<LoadedFont>,
        faces: &SampleFaces,
        width: f32,
    ) -> (Vec<SampleChip>, f32) {
        let pad_x = self.ui_px(18.0);
        let gap = self.ui_px(12.0);
        let chip_height = self.font_sample_chip_height(font);
        let Some(parts) = &faces.parts else {
            return (Vec::new(), 0.0);
        };
        let mut chips = Vec::with_capacity(FONT_SAMPLE.len());
        let (mut chip_x, mut chip_y) = (0.0, 0.0);
        for (part, families) in FONT_SAMPLE.iter().zip(parts) {
            // Every face that draws some of it: an umlaut can come from
            // another font than the letters around it.
            let caption = if families.is_empty() {
                crate::i18n::tr("settings-font-sample-missing")
            } else {
                families
                    .iter()
                    .map(|family| display_family(family))
                    .collect::<Vec<_>>()
                    .join(" + ")
            };
            let inner = self
                .measure_text_width(font, part)
                .max(self.measure_text_width(&self.body_font, &caption))
                .min((width - pad_x * 2.0).max(0.0));
            let chip_width = inner + pad_x * 2.0;
            if chip_x > 0.0 && chip_x + chip_width > width {
                chip_x = 0.0;
                chip_y += chip_height + gap;
            }
            chips.push(SampleChip {
                x: chip_x,
                y: chip_y,
                width: chip_width,
                caption,
            });
            chip_x += chip_width + gap;
        }
        (chips, chip_y + chip_height)
    }

    /// `FONT_SAMPLE` in `font`, each part on a chip over the name of the
    /// family that draws it. Returns where it ends.
    fn paint_font_sample(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        faces: &SampleFaces,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let (chips, height) = self.layout_font_sample(font, faces, width);
        let palette = self.palette();
        let fill = self.chrome_palette.group_bg;
        let body_font = Rc::clone(&self.body_font);
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let part_height = metrics.cell_size.height as f32;
        let chip_height = self.font_sample_chip_height(font);
        let pad_x = self.ui_px(18.0);
        let pad_y = self.ui_px(14.0);
        let parts = faces.parts.as_deref().unwrap_or(&[]);
        for ((chip, part), families) in chips.iter().zip(FONT_SAMPLE).zip(parts) {
            let (left, top) = (x + chip.x, y + chip.y);
            let room = chip.width - pad_x * 2.0;
            self.draw_rounded_rect(
                layers,
                0,
                left,
                top,
                chip.width,
                chip_height,
                fill,
                self.ui_px(20.0),
            )?;
            self.draw_text(
                layers,
                font,
                left + pad_x,
                self.text_top_for(&metrics, top + pad_y),
                part,
                if families.is_empty() {
                    palette.muted_text
                } else {
                    palette.text
                },
                room,
            )?;
            let caption = self.text_with_ellipsis(&body_font, &chip.caption, room);
            self.draw_text(
                layers,
                &body_font,
                left + pad_x,
                top + pad_y + part_height + self.ui_px(4.0),
                &caption,
                palette.muted_text,
                room,
            )?;
        }
        Ok(y + height)
    }

    /// The recommended sets, each its name over the fonts it puts in the
    /// list, under a word on what choosing one does. Returns where it ends.
    fn paint_font_preset_list(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        top: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let card = self.floating_card_palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        let padding = self.ui_px(16.0);
        let picker = self.ui.font_picker.as_ref().unwrap();
        let presets = picker.presets.clone();
        let chosen = picker.preset.as_ref().map(|preset| preset.language);

        // Where the search field is for a single font.
        let note = crate::i18n::tr("settings-font-presets-note");
        let mut y = top + self.ui_px(4.0);
        y += self.draw_row_note_in_lines(layers, x, y, &note, width, 4)?
            + self.description_line_step()
            + self.ui_px(12.0);

        let label_height = self.dropdown_row_height();
        let row_height = label_height + self.description_line_step();
        let list_height = row_height * presets.len().max(1) as f32 + padding * 2.0;
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            list_height,
            card.control_bg,
            card.control_border,
            self.ui_px(DROPDOWN_RADIUS),
        )?;
        let inset = self.dropdown_label_inset();
        let room = (width - inset - padding).max(0.0);
        for (index, preset) in presets.iter().enumerate() {
            let row = rect(
                x,
                y + padding + index as f32 * row_height,
                width,
                row_height,
            );
            let selected = chosen == Some(preset.language);
            if selected {
                self.draw_rounded_rect(
                    layers,
                    0,
                    row.min_x() + self.ui_px(8.0),
                    row.min_y() + self.ui_px(2.0),
                    row.size.width - self.ui_px(16.0),
                    row.size.height - self.ui_px(4.0),
                    crate::ui::color::mix(card.control_bg, card.text, 0.10),
                    self.ui_px(DROPDOWN_ROW_RADIUS),
                )?;
            }
            // The name and the fonts under it, centred together in the row.
            let label_y =
                self.control_text_y(row.min_y(), row_height - self.description_line_step());
            let action = SettingsAction::FontPickerPreset(preset.language);
            let color =
                self.paint_dropdown_row_on_layer(layers, &card, row, label_y, action, selected, 0)?;
            let label = crate::i18n::tr(preset.language.label_key());
            let label = self.text_with_ellipsis(&ui_font, &label, room);
            self.draw_text(layers, &ui_font, x + inset, label_y, &label, color, room)?;
            let families = self.text_with_ellipsis(&body_font, &preset.families.join(" · "), room);
            self.draw_text(
                layers,
                &body_font,
                x + inset,
                label_y + self.description_line_step(),
                &families,
                card.muted_text,
                room,
            )?;
        }
        if presets.is_empty() {
            self.draw_text(
                layers,
                &ui_font,
                x + padding,
                y + padding + self.ui_px(20.0),
                &crate::i18n::tr(if self.ui.font_catalog.loading {
                    "settings-font-loading"
                } else if self.ui.font_catalog.names.is_none() {
                    "settings-font-list-error"
                } else {
                    "settings-font-presets-none"
                }),
                palette.muted_text,
                width - padding * 2.0,
            )?;
        }
        Ok(y + list_height)
    }

    /// The search field over the installed families, a page of them to
    /// choose from and the arrows to page. Returns where it ends.
    fn paint_font_catalog_list(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        top: f32,
        list_width: f32,
        query: &str,
        excluded: &[String],
    ) -> anyhow::Result<f32> {
        let picker = self.ui.font_picker.as_ref().unwrap();
        let target = picker.target;
        let slot = picker.slot;
        let draft = picker.draft.clone();
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let row_height = self.dropdown_row_height();
        let gap = self.ui_px(20.0);
        let padding = self.ui_px(16.0);
        let search_rect = rect(x, top, list_width, self.ui_px(CONTROL_HEIGHT));
        self.paint_text_input(
            layers,
            0,
            TextInputSpec {
                text: query,
                placeholder: &crate::i18n::tr("settings-font-search"),
                rect: search_rect,
                focused: self.ui.interaction.focused == Some(SettingsAction::FontPickerSearch),
                selected_all: self.ui.font_picker.as_ref().unwrap().search.selected_all,
                action: SettingsAction::FontPickerSearch,
            },
        )?;
        self.draw_svg_icon(
            layers,
            SvgIcon::Search,
            x + self.ui_px(18.0),
            top + self.ui_px(16.0),
            self.ui_px(24.0),
            palette.muted_text,
        )?;
        let list_y = search_rect.max_y() + self.ui_px(12.0);
        // The default is the main font's alone: a fallback is a family.
        let default_row = slot == FontSlot::Main;
        let list_height =
            row_height * (PAGE_SIZE + usize::from(default_row)) as f32 + padding * 2.0;
        let card = self.floating_card_palette();
        self.draw_rounded_frame(
            layers,
            0,
            x,
            list_y,
            list_width,
            list_height,
            card.control_bg,
            card.control_border,
            self.ui_px(DROPDOWN_RADIUS),
        )?;
        let picker = self.ui.font_picker.as_mut().unwrap();
        let names = self.ui.font_catalog.names.as_deref().unwrap_or(&[]);
        if picker.filter_query.as_deref() != Some(query) || picker.catalog_len != names.len() {
            let prefer_mono = target == FontTarget::Terminal && slot == FontSlot::Main;
            picker.filtered = ranked_indices(names, query, prefer_mono, excluded);
            picker.filter_query = Some(query.to_string());
            picker.catalog_len = names.len();
        }
        let result_count = picker.filtered.len();
        let last_page = result_count.saturating_sub(1) / PAGE_SIZE;
        let page = picker.page.min(last_page);
        picker.page = page;
        let indices: Vec<usize> = picker
            .filtered
            .iter()
            .skip(page * PAGE_SIZE)
            .take(PAGE_SIZE)
            .copied()
            .collect();
        let mut options = Vec::with_capacity(PAGE_SIZE + 1);
        if default_row {
            options.push((
                crate::i18n::tr("settings-font-default"),
                SettingsAction::FontPickerDefault,
                draft.is_none(),
            ));
        }
        for &index in &indices {
            let name = names[index].clone();
            let selected = draft.as_deref() == Some(name.as_str());
            options.push((
                name,
                SettingsAction::FontPickerChoose(index as u16, self.ui.font_catalog.generation),
                selected,
            ));
        }
        for (index, (label, action, selected)) in options.iter().enumerate() {
            let row = rect(
                x,
                list_y + padding + index as f32 * row_height,
                list_width,
                row_height,
            );
            if *selected {
                self.draw_rounded_rect(
                    layers,
                    0,
                    row.min_x() + self.ui_px(8.0),
                    row.min_y() + self.ui_px(2.0),
                    row.size.width - self.ui_px(16.0),
                    row.size.height - self.ui_px(4.0),
                    crate::ui::color::mix(card.control_bg, card.text, 0.10),
                    self.ui_px(DROPDOWN_ROW_RADIUS),
                )?;
            }
            let label_y = self.control_text_y(row.min_y(), row_height);
            let color = self
                .paint_dropdown_row_on_layer(layers, &card, row, label_y, *action, *selected, 0)?;
            let inset = self.dropdown_label_inset();
            let label = self.text_with_ellipsis(&ui_font, label, list_width - inset - padding);
            self.draw_text(
                layers,
                &ui_font,
                x + inset,
                label_y,
                &label,
                color,
                list_width - inset - padding,
            )?;
        }
        if result_count == 0 {
            self.draw_text(
                layers,
                &ui_font,
                x + padding,
                list_y + padding + row_height * usize::from(default_row) as f32 + gap,
                &crate::i18n::tr(if self.ui.font_catalog.loading {
                    "settings-font-loading"
                } else {
                    "settings-font-no-results"
                }),
                palette.muted_text,
                list_width - padding * 2.0,
            )?;
        }
        let pagination_y = list_y + list_height + self.ui_px(8.0);
        let small = self.ui_px(44.0);
        self.paint_icon_button(
            layers,
            x,
            pagination_y,
            small,
            SvgIcon::ArrowLeft,
            SettingsAction::FontPickerPage(-1),
        )?;
        self.paint_icon_button(
            layers,
            x + list_width - small,
            pagination_y,
            small,
            SvgIcon::ArrowRight,
            SettingsAction::FontPickerPage(1),
        )?;
        self.draw_text(
            layers,
            &ui_font,
            x + small + padding,
            self.control_text_y(pagination_y, small),
            &format!("{} / {}", page + 1, last_page + 1),
            palette.muted_text,
            list_width - small * 2.0 - padding * 2.0,
        )?;
        Ok(pagination_y + small)
    }

    fn paint_font_picker(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        top: f32,
        width: f32,
    ) -> anyhow::Result<f32> {
        let config = configuration();
        if self.fonts.config().generation() != config.generation() {
            self.reload_configured_fonts(&config)?;
        }
        // The configuration's main font, not to offer as a fallback, as it
        // is now: the configuration may have been reloaded since.
        let picker = self.ui.font_picker.as_mut().unwrap();
        if picker.default_main_generation != Some(config.generation()) {
            picker.default_main = picker
                .base
                .primary
                .is_none()
                .then(|| Self::default_main_family(picker.target, &config))
                .flatten();
            picker.default_main_generation = Some(config.generation());
            picker.filter_query = None;
        }
        let picker = self.ui.font_picker.as_ref().unwrap();
        let query = picker.search.text().to_string();
        let target = picker.target;
        let slot = picker.slot;
        let draft = picker.draft.clone();
        let excluded = picker.excluded();
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let gap = self.ui_px(20.0);
        let padding = self.ui_px(16.0);
        let side_by_side = width >= self.ui_px(800.0);
        let list_width = if side_by_side {
            (width * 0.44).min(self.ui_px(520.0))
        } else {
            width
        };
        let list_bottom = if slot == FontSlot::Recommended {
            self.paint_font_preset_list(layers, x, top, list_width)?
        } else {
            self.paint_font_catalog_list(layers, x, top, list_width, &query, &excluded)?
        };
        let preview_x = if side_by_side {
            x + list_width + gap
        } else {
            x
        };
        let preview_y = if side_by_side { top } else { list_bottom + gap };
        let preview_width = if side_by_side {
            width - list_width - gap
        } else {
            width
        };
        let inner_width = preview_width - padding * 2.0;

        // What the preview holds decides how tall it is, so it is loaded
        // and laid out before its card is drawn.
        self.ui
            .font_picker
            .as_mut()
            .unwrap()
            .load_preview(&self.fonts, &config);
        let preview = self.ui.font_picker.as_ref().unwrap().preview.clone();
        // Which font draws each part of the mixed line shows on the sample
        // over the picker, which follows the choice while it is open.
        let mixed = FONT_SAMPLE.join(" ");
        let samples: [&str; 4] = match target {
            FontTarget::Interface => ["ThinkTerm", "Aa Bb 0123456789", &mixed, "O0 Il1  → ≠ {} []"],
            FontTarget::Terminal => [
                "~/project $ git status",
                "On branch main",
                &mixed,
                "O0 Il1  → ≠ {} []",
            ],
        };
        let step = preview
            .as_ref()
            .map(|font| (font.metrics().cell_height.get() as f32).max(self.ui_px(34.0)));
        // A set's fonts in full under its name, which its row may have cut.
        let set_lines = match (
            slot,
            self.ui.font_picker.as_ref().and_then(|p| p.preset.as_ref()),
        ) {
            (FontSlot::Recommended, Some(preset)) => {
                let body_font = Rc::clone(&self.body_font);
                self.capped_lines(&body_font, &preset.families.join(" → "), inner_width, 3)
            }
            _ => Vec::new(),
        };
        let samples_top = self.ui_px(120.0) + self.description_line_step() * set_lines.len() as f32;
        let content_height = samples_top
            + padding
            + step.map_or(self.ui_px(40.0), |step| step * samples.len() as f32);
        let preview_height = if side_by_side {
            (list_bottom - top).max(content_height)
        } else {
            self.ui_px(270.0).max(content_height)
        };
        self.draw_rounded_frame(
            layers,
            0,
            preview_x,
            preview_y,
            preview_width,
            preview_height,
            palette.card_bg,
            palette.control_border,
            self.ui_px(DROPDOWN_RADIUS),
        )?;
        self.draw_text(
            layers,
            &ui_font,
            preview_x + padding,
            preview_y + padding,
            &crate::i18n::tr("settings-font-preview"),
            palette.muted_text,
            inner_width,
        )?;
        let preset = self
            .ui
            .font_picker
            .as_ref()
            .and_then(|picker| picker.preset.as_ref())
            .map(|preset| crate::i18n::tr(preset.language.label_key()));
        let heading = match (&draft, slot) {
            (Some(family), _) => Some(family.clone()),
            (None, FontSlot::Main) => Some(crate::i18n::tr("settings-font-default")),
            (None, FontSlot::Recommended) => preset,
            (None, _) => None,
        };
        if let Some(heading) = heading {
            self.draw_text(
                layers,
                &ui_font,
                preview_x + padding,
                preview_y + padding + self.ui_px(38.0),
                &heading,
                palette.text,
                inner_width,
            )?;
        }
        if !set_lines.is_empty() {
            let body_font = Rc::clone(&self.body_font);
            self.draw_text_lines(
                layers,
                &body_font,
                preview_x + padding,
                preview_y
                    + padding
                    + self.ui_px(38.0)
                    + self.description_line_step()
                    + self.ui_px(6.0),
                &set_lines,
                palette.muted_text,
                inner_width,
            )?;
        }
        let mut y = preview_y + samples_top;
        if let (Some(font), Some(step)) = (preview, step) {
            for text in samples {
                self.draw_text(
                    layers,
                    &font,
                    preview_x + padding,
                    y,
                    text,
                    palette.text,
                    inner_width,
                )?;
                y += step;
            }
        } else {
            self.draw_text(
                layers,
                &ui_font,
                preview_x + padding,
                y,
                &crate::i18n::tr("settings-font-unavailable"),
                palette.muted_text,
                inner_width,
            )?;
        }
        let footer_y = list_bottom.max(preview_y + preview_height) + gap;
        let apply_label = crate::i18n::tr(if slot == FontSlot::Add {
            "settings-font-add"
        } else {
            "settings-font-apply"
        });
        let cancel_label = crate::i18n::tr("settings-font-cancel");
        let apply_width = self
            .button_width_for_label(&apply_label, 130.0)
            .min(width * 0.35);
        let cancel_width = self
            .button_width_for_label(&cancel_label, 130.0)
            .min(width * 0.35);
        let apply_x = x + width - apply_width;
        self.draw_button(
            layers,
            apply_x - gap - cancel_width,
            footer_y,
            cancel_width,
            &cancel_label,
            SettingsAction::FontPickerCancel,
        )?;
        if !self.font_picker_can_apply() {
            self.draw_button_enabled(
                layers,
                apply_x,
                footer_y,
                apply_width,
                &apply_label,
                SettingsAction::FontPickerApply,
                false,
            )?;
        } else {
            let ctx = crate::ui::draw::DrawContext::new(
                self.render_state
                    .as_ref()
                    .context("settings render state")?,
                self.dimensions,
                &self.metrics,
            );
            let action = SettingsAction::FontPickerApply;
            let state = if self.ui.interaction.pressed == Some(action) {
                ControlState::Pressed
            } else if self.ui.interaction.hovered == Some(action) {
                ControlState::Hovered
            } else {
                ControlState::Normal
            };
            let button_rect = rect(apply_x, footer_y, apply_width, self.ui_px(CONTROL_HEIGHT));
            crate::ui::widgets::draw_button_on_layer(
                &ctx,
                layers,
                &ui_font,
                &mut self.ui_context,
                self.chrome_palette,
                ButtonSpec {
                    label: &apply_label,
                    rect: button_rect,
                    action,
                    state,
                    kind: WidgetKind::Button,
                    variant: ButtonVariant::Primary,
                },
                0,
            )?;
        }
        Ok(footer_y + self.ui_px(CONTROL_HEIGHT) + gap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_drops_old_directory_results_and_releases_loaded_names() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../assets/fonts/FiraCode-Regular.ttf");
        std::fs::copy(source, directory.path().join("example.ttf"))?;
        let empty = config::ConfigHandle::default_config().adjusted(|cfg| {
            cfg.font_locator = config::FontLocatorSelection::ConfigDirsOnly;
            cfg.font_dirs.clear();
        });
        let added = empty.adjusted(|cfg| cfg.font_dirs.push(directory.path().into()));
        let fonts = FontConfiguration::new(Some(empty.clone()), 96)?;
        let mut catalog = FontCatalog::default();
        let old = catalog.begin().unwrap();
        let old_job = fonts.font_family_catalog_job();
        fonts.config_changed(&added)?;
        catalog.invalidate();
        assert!(catalog.begin().is_none());
        assert!(!catalog.finish(old, old_job())?);
        let fresh = catalog.begin().unwrap();
        assert!(catalog.finish(fresh, fonts.font_family_catalog_job()())?);
        assert!(catalog
            .names
            .as_ref()
            .unwrap()
            .iter()
            .any(|family| family == "Fira Code"));
        assert!(catalog.begin().is_none());
        fonts.config_changed(&empty)?;
        catalog.invalidate();
        assert!(catalog.names.is_none());
        let fresh = catalog.begin().unwrap();
        assert!(catalog.finish(fresh, fonts.font_family_catalog_job()())?);
        assert!(!catalog
            .names
            .as_ref()
            .unwrap()
            .iter()
            .any(|family| family == "Fira Code"));
        Ok(())
    }

    #[test]
    fn default_preview_tracks_config_and_fallback_at_the_same_dpi() -> anyhow::Result<()> {
        let _turn = wezterm_font::harfbuzz_turn();
        let configs = config::Configuration::new();
        let with_family = |family: &str| {
            configs.get().adjusted(|cfg| {
                cfg.font_locator = config::FontLocatorSelection::ConfigDirsOnly;
                cfg.font_dirs =
                    vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/fonts")];
                cfg.font.set_primary_family(family);
            })
        };
        let base = with_family("Roboto");
        let fonts = FontConfiguration::new(Some(base.clone()), 96)?;
        let mut picker = FontPicker::new(
            FontTarget::Terminal,
            FontSlot::Main,
            FontStack::default(),
            None,
        );
        picker.load_preview(&fonts, &base);
        let original = picker.preview.clone().expect("initial preview");
        assert_eq!(original.clone_handles()[0].names().family, "Roboto");

        configs.use_defaults();
        let reloaded = with_family("JetBrains Mono");
        fonts.config_changed(&reloaded)?;
        picker.load_preview(&fonts, &reloaded);
        let changed = picker.preview.clone().expect("preview after reload");
        assert!(!Rc::ptr_eq(&original, &changed));
        assert_eq!(changed.clone_handles()[0].names().family, "JetBrains Mono");

        let policy = reloaded.adjusted(|cfg| cfg.fontconfig_prefer_system_fallback = true);
        fonts.config_changed(&policy)?;
        picker.load_preview(&fonts, &reloaded);
        let fallback = picker
            .preview
            .as_ref()
            .expect("preview after fallback change");
        assert!(!Rc::ptr_eq(&changed, fallback));
        Ok(())
    }

    #[test]
    fn font_preview_tracks_display_dpi_without_reloading_at_the_same_scale() -> anyhow::Result<()> {
        let _turn = wezterm_font::harfbuzz_turn();
        let config = config::ConfigHandle::default_config();
        for target in [FontTarget::Interface, FontTarget::Terminal] {
            let fonts = FontConfiguration::new(Some(config.clone()), 96)?;
            let mut picker = FontPicker::new(
                target,
                FontSlot::Main,
                FontStack::new(Some("JetBrains Mono"), &[]),
                None,
            );
            picker.load_preview(&fonts, &config);
            let original = picker.preview.clone().expect("font preview");
            picker.load_preview(&fonts, &config);
            assert!(Rc::ptr_eq(&original, picker.preview.as_ref().unwrap()));

            fonts.change_scaling(fonts.get_font_scale(), 192);
            picker.load_preview(&fonts, &config);
            let scaled = picker.preview.clone().expect("scaled font preview");
            assert!(!Rc::ptr_eq(&original, &scaled));
            assert!(scaled.metrics().cell_height > original.metrics().cell_height);

            fonts.change_scaling(fonts.get_font_scale(), 96);
            picker.load_preview(&fonts, &config);
            let restored = picker.preview.as_ref().expect("restored font preview");
            assert_eq!(
                restored.metrics().cell_height,
                original.metrics().cell_height
            );
            assert!(!Rc::ptr_eq(&scaled, restored));
        }
        Ok(())
    }

    #[test]
    fn search_preserves_catalog_indices_and_prioritizes_terminal_families() {
        let names = vec![
            "Example Sans".into(),
            "Example Mono".into(),
            "Other Face".into(),
        ];
        assert_eq!(ranked_indices(&names, "example", true, &[]), vec![1, 0]);
        assert_eq!(ranked_indices(&names, "example", false, &[]), vec![0, 1]);
        assert!(ranked_indices(&names, "absent", true, &[]).is_empty());
        assert_eq!(
            ranked_indices(&names, "", false, &["example mono".into()]),
            vec![0, 2],
            "a family the list has is not offered again"
        );
    }

    #[test]
    fn each_slot_puts_the_draft_in_its_place() {
        let base = FontStack::new(Some("Main"), &["First".to_string(), "Second".to_string()]);
        let stack = |slot, draft: Option<&str>| {
            let mut picker = FontPicker::new(FontTarget::Terminal, slot, base.clone(), None);
            picker.draft = draft.map(str::to_string);
            let stack = picker.draft_stack();
            (stack.primary, stack.fallbacks)
        };
        let some = |name: &str| Some(name.to_string());
        let list = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            stack(FontSlot::Main, Some("Other")),
            (some("Other"), list(&["First", "Second"]))
        );
        assert_eq!(
            stack(FontSlot::Main, Some("Second")),
            (some("Second"), list(&["First"])),
            "a fallback made the main font leaves the fallbacks"
        );
        assert_eq!(
            stack(FontSlot::Main, None),
            (None, list(&["First", "Second"]))
        );
        assert_eq!(
            stack(FontSlot::Add, Some("Third")),
            (some("Main"), list(&["First", "Second", "Third"]))
        );
        assert_eq!(
            stack(FontSlot::Add, None),
            (some("Main"), list(&["First", "Second"]))
        );
        assert_eq!(
            stack(FontSlot::Fallback(1), Some("Other")),
            (some("Main"), list(&["First", "Other"]))
        );

        let picker = FontPicker::new(
            FontTarget::Terminal,
            FontSlot::Fallback(0),
            base.clone(),
            None,
        );
        assert_eq!(picker.draft.as_deref(), Some("First"));
        assert_eq!(picker.excluded(), ["main", "second"]);
        let picker = FontPicker::new(FontTarget::Terminal, FontSlot::Add, base.clone(), None);
        assert!(!picker.can_apply(), "nothing to add yet");
        assert_eq!(picker.excluded(), ["main", "first", "second"]);
        let picker = FontPicker::new(FontTarget::Terminal, FontSlot::Main, base.clone(), None);
        assert!(picker.excluded().is_empty());

        // Left to the configuration, the main font is still not offered:
        // the list would pass it by.
        let unset = FontStack::new(None, &base.fallbacks);
        let picker = FontPicker::new(
            FontTarget::Terminal,
            FontSlot::Add,
            unset.clone(),
            Some("JetBrains Mono".to_string()),
        );
        assert_eq!(picker.excluded(), ["jetbrains mono", "first", "second"]);
        let picker = FontPicker::new(
            FontTarget::Terminal,
            FontSlot::Main,
            unset,
            Some("JetBrains Mono".to_string()),
        );
        assert!(picker.excluded().is_empty());
    }

    #[test]
    fn a_recommended_set_goes_ahead_of_the_fallbacks() {
        let list =
            |names: &[&str]| -> Vec<String> { names.iter().map(|name| name.to_string()).collect() };
        let base = FontStack::new(Some("Main"), &list(&["First", "Second"]));
        let mut picker = FontPicker::new(
            FontTarget::Terminal,
            FontSlot::Recommended,
            base.clone(),
            None,
        );
        assert_eq!(picker.draft, None);
        assert!(picker.excluded().is_empty());
        assert_eq!(picker.draft_stack(), base, "no set chosen yet");
        assert!(!picker.can_apply());
        let set = |families: &[&str]| FontPreset {
            language: CjkLanguage::Japanese,
            families: list(families),
        };
        // The main font in the set is the main font still, not a fallback;
        // the list's own fallbacks come after the set.
        picker.preset = Some(set(&["JP Font", "Main", "SC Font"]));
        assert_eq!(
            picker.draft_stack(),
            FontStack::new(
                Some("Main"),
                &list(&["JP Font", "SC Font", "First", "Second"])
            )
        );
        // Applied to the list as it is by then.
        let mut now = FontStack::new(Some("Other"), &list(&["Added Meanwhile", "sc font"]));
        picker.edit().apply(&mut now);
        assert_eq!(now.primary.as_deref(), Some("Other"));
        assert_eq!(
            now.fallbacks,
            ["JP Font", "Main", "SC Font", "Added Meanwhile"]
        );
        assert_eq!(picker.edit().adds(), None);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn the_sets_follow_the_catalogue_and_keep_the_language_chosen() {
        let names = |families: &[&str]| -> Vec<String> {
            families.iter().map(|family| family.to_string()).collect()
        };
        let mut picker = FontPicker::new(
            FontTarget::Terminal,
            FontSlot::Recommended,
            FontStack::default(),
            None,
        );
        picker.refresh_presets(None, 0);
        assert!(picker.presets.is_empty(), "nothing before the catalogue");
        let catalogue = names(&["Noto Sans Mono CJK JP", "Noto Sans Mono CJK KR"]);
        picker.refresh_presets(Some(&catalogue), 0);
        assert_eq!(picker.presets.len(), 2);
        assert_eq!(
            picker.preset.as_ref(),
            picker.presets.first(),
            "the first set"
        );
        // Not chosen by hand, the first set follows a new catalogue.
        let more = names(&[
            "Noto Sans Mono CJK JP",
            "Noto Sans Mono CJK KR",
            "Noto Sans Mono CJK SC",
            "Noto Sans Mono CJK TC",
        ]);
        picker.refresh_presets(Some(&more), 1);
        assert_eq!(picker.presets.len(), 4);
        assert_eq!(picker.preset.as_ref(), picker.presets.first());
        // While the catalogue loads again, nothing to show or apply.
        picker.refresh_presets(None, 2);
        assert!(picker.presets.is_empty());
        assert_eq!(picker.preset, None);

        picker.chosen = Some(CjkLanguage::Korean);
        let catalogue = names(&[
            "Noto Sans Mono CJK JP",
            "Noto Sans Mono CJK KR",
            "Noto Sans Mono CJK SC",
        ]);
        picker.refresh_presets(Some(&catalogue), 3);
        assert_eq!(picker.presets.len(), 3);
        let shown = picker.preset.as_ref().unwrap();
        assert_eq!(shown.language, CjkLanguage::Korean);
        assert_eq!(
            shown.families,
            [
                "Noto Sans Mono CJK KR",
                "Noto Sans Mono CJK SC",
                "Noto Sans Mono CJK JP"
            ],
            "the set chosen, as installed now"
        );
    }

    #[test]
    fn the_sample_names_the_first_font_of_the_list_with_each_part() -> anyhow::Result<()> {
        let _turn = wezterm_font::harfbuzz_turn();
        // Roboto has no box drawing; JetBrains Mono, next in the list, does.
        let config = config::ConfigHandle::default_config().adjusted(|cfg| {
            // Only the bundled fonts: the sample's Chinese and Japanese are
            // missing, which must not reach the desktop as a notification.
            cfg.warn_about_missing_glyphs = false;
            cfg.font_locator = config::FontLocatorSelection::ConfigDirsOnly;
            cfg.font_dirs =
                vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/fonts")];
            cfg.font = config::TextStyle {
                font: vec![config::FontAttributes::new("Roboto")],
                foreground: None,
            };
        });
        let fonts = FontConfiguration::new(Some(config.clone()), 96)?;
        let listed = |fallbacks: &[&str]| -> anyhow::Result<Rc<LoadedFont>> {
            let fallbacks: Vec<String> = fallbacks.iter().map(|f| f.to_string()).collect();
            let stack = FontStack::new(None, &fallbacks);
            let config = config.adjusted(|cfg| {
                cfg.font = crate::native_settings::terminal_font_style(&config, &stack);
            });
            fonts.terminal_font_uncached(&config, 14.0)
        };
        for (order, expected) in [
            (["JetBrains Mono", "Fira Code"], "JetBrains Mono"),
            (["Fira Code", "JetBrains Mono"], "Fira Code"),
        ] {
            let font = listed(&order)?;
            let drawn = font.blocking_shape("─", None, Direction::LeftToRight, None, None)?;
            let handles = font.clone_handles();
            assert_eq!(handles[0].names().family, "Roboto");
            assert_eq!(
                handles[drawn[0].font_idx].names().family,
                expected,
                "{order:?}"
            );
            let faces = SampleFaces::of(&font, true);
            let parts = faces.parts.as_ref().expect("shaped");
            assert_eq!(parts[0], ["Roboto"], "Latin from the main font");
            assert!(faces.has("Fira Code") && faces.has("JetBrains Mono"));
            assert!(!faces.has("Not Installed Anywhere"));
            // A part two faces draw names both, in the order they come.
            let mixed: Vec<String> = faces_drawing(&font, "A─")
                .into_iter()
                .map(|index| font.clone_handles()[index].names().family.clone())
                .collect();
            assert_eq!(mixed, ["Roboto", expected], "{order:?}");
            assert_eq!(faces.stand_in_for("Roboto"), None);
        }
        let font = listed(&["Not Installed Anywhere"])?;
        let faces = SampleFaces::of(&font, false);
        assert!(faces.parts.is_none(), "nothing shaped unless asked");
        assert!(!faces.has("Not Installed Anywhere"));
        // The loader also takes a face by its PostScript name.
        assert!(faces.has("Roboto-Regular"));
        Ok(())
    }

    #[test]
    fn a_missing_main_font_names_what_stands_in() -> anyhow::Result<()> {
        let _turn = wezterm_font::harfbuzz_turn();
        let config = config::ConfigHandle::default_config().adjusted(|cfg| {
            cfg.font_locator = config::FontLocatorSelection::ConfigDirsOnly;
            cfg.font_dirs =
                vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/fonts")];
        });
        let fonts = FontConfiguration::new(Some(config.clone()), 96)?;
        let stack = FontStack::new(Some("Not Installed Anywhere"), &["Fira Code".to_string()]);
        let font = fonts.terminal_font_uncached(
            &config.adjusted(|cfg| {
                cfg.font = crate::native_settings::terminal_font_style(&config, &stack);
            }),
            14.0,
        )?;
        let faces = SampleFaces::of(&font, false);
        let used = faces.stand_in_for("Not Installed Anywhere");
        assert_eq!(
            used.as_deref(),
            Some(font.clone_handles()[0].names().family.as_str())
        );
        assert_eq!(
            used.as_deref(),
            Some("JetBrains Mono"),
            "the bundled font, not the list's first"
        );
        // A generic family is resolved, not missing.
        assert_eq!(faces.stand_in_for("monospace"), None);
        assert_eq!(faces.stand_in_for(" Sans-Serif "), None);
        // A family written in another case is the one loaded.
        let font = fonts.terminal_font_uncached(&config, 14.0)?;
        assert_eq!(
            SampleFaces::of(&font, false).stand_in_for("jetbrains mono"),
            None
        );
        Ok(())
    }

    #[test]
    fn edits_go_by_name_into_the_list_as_it_is_now() {
        let opened = FontStack::new(Some("Main"), &["First".to_string(), "Second".to_string()]);
        // Changed elsewhere while the picker was open.
        let now = || FontStack {
            primary: Some("Main".to_string()),
            fallbacks: ["Other", "First", "Second", "New"]
                .iter()
                .map(|family| family.to_string())
                .collect(),
        };
        let picked = |slot, draft: &str| {
            let mut picker = FontPicker::new(FontTarget::Terminal, slot, opened.clone(), None);
            picker.draft = Some(draft.to_string());
            let mut stack = now();
            picker.edit().apply(&mut stack);
            stack
        };
        assert_eq!(
            picked(FontSlot::Fallback(1), "Replacement").fallbacks,
            ["Other", "First", "Replacement", "New"],
            "the family the picker opened on is replaced, wherever it is now"
        );
        let main = picked(FontSlot::Main, "Picked");
        assert_eq!(main.primary.as_deref(), Some("Picked"));
        assert_eq!(main.fallbacks, now().fallbacks, "the others are kept");
        assert_eq!(
            picked(FontSlot::Add, "Added").fallbacks,
            ["Other", "First", "Second", "New", "Added"]
        );
        // The main font meanwhile: no place for it among the fallbacks,
        // which saving checks before it changes anything.
        let mut picker = FontPicker::new(
            FontTarget::Terminal,
            FontSlot::Fallback(0),
            opened.clone(),
            None,
        );
        picker.draft = Some("Picked".to_string());
        let mut stack = FontStack::new(Some("Picked"), &["First".to_string()]);
        picker.edit().apply(&mut stack);
        let stack = FontStack::new(stack.primary.as_deref(), &stack.fallbacks);
        assert!(!stack.has_fallback("Picked"));

        // Gone meanwhile: the choice goes after the others, not nowhere.
        let mut picker = FontPicker::new(FontTarget::Terminal, FontSlot::Fallback(0), opened, None);
        picker.draft = Some("Replacement".to_string());
        assert_eq!(picker.edit().adds(), Some("Replacement"));
        let mut stack = FontStack::new(Some("Main"), &["Second".to_string()]);
        picker.edit().apply(&mut stack);
        assert_eq!(stack.fallbacks, ["Second", "Replacement"]);
        // Nothing picked, nothing to apply or add.
        picker.draft = None;
        assert_eq!(picker.edit().adds(), None);
        let mut stack = FontStack::new(Some("Main"), &["Second".to_string()]);
        picker.edit().apply(&mut stack);
        assert_eq!(stack.fallbacks, ["Second"]);

        let mut stack = now();
        move_fallback(&mut stack, "second", true);
        assert_eq!(stack.fallbacks, ["Other", "Second", "First", "New"]);
        move_fallback(&mut stack, "New", false);
        assert_eq!(
            stack.fallbacks,
            ["Other", "Second", "First", "New"],
            "already last"
        );
        move_fallback(&mut stack, "Absent", true);
        remove_fallback(&mut stack, "first");
        assert_eq!(stack.fallbacks, ["Other", "Second", "New"]);
    }
}
