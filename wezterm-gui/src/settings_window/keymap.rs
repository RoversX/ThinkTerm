//! Settings → Keymap: the shortcuts drawn on a keyboard, and changing them.
//!
//! What the page shows is built when it is entered and after each change,
//! and let go when another page is shown; painting only reads it.

use super::*;
use crate::commands::CommandDef;
use crate::inputmap::InputMap;
use config::keyassignment::KeyAssignment;
use std::collections::HashMap;
use window::{KeyCode, Modifiers, UIKeyCapRendering};

/// A row of the drawn keyboard is this many key units wide.
const ROW_UNITS: f32 = 14.5;
/// The most rows the search list shows; a narrower search shows the rest.
const LIST_LIMIT: usize = 50;
const SUPER_LEGEND: &str = if cfg!(windows) { "Win" } else { "Super" };

enum CapKind {
    Key(KeyCode),
    Modifier(Modifiers),
    Inert,
}

struct Cap {
    kind: CapKind,
    /// The legend on macOS, and on other systems.
    mac: &'static str,
    pc: &'static str,
    /// In key units.
    width: f32,
    /// 1 and 2 are the upper and lower halves of the stacked arrows, which
    /// share one column.
    half: u8,
}

const fn key(c: char, legend: &'static str) -> Cap {
    Cap {
        kind: CapKind::Key(KeyCode::Char(c)),
        mac: legend,
        pc: legend,
        width: 1.0,
        half: 0,
    }
}

const fn named(code: KeyCode, mac: &'static str, pc: &'static str, width: f32) -> Cap {
    Cap {
        kind: CapKind::Key(code),
        mac,
        pc,
        width,
        half: 0,
    }
}

const fn modifier(mods: Modifiers, mac: &'static str, pc: &'static str, width: f32) -> Cap {
    Cap {
        kind: CapKind::Modifier(mods),
        mac,
        pc,
        width,
        half: 0,
    }
}

const fn inert(mac: &'static str, pc: &'static str, width: f32) -> Cap {
    Cap {
        kind: CapKind::Inert,
        mac,
        pc,
        width,
        half: 0,
    }
}

const fn arrow(code: KeyCode, legend: &'static str, half: u8) -> Cap {
    Cap {
        kind: CapKind::Key(code),
        mac: legend,
        pc: legend,
        width: 1.0,
        half,
    }
}

static NUMBER_ROW: [Cap; 14] = [
    key('`', "`"),
    key('1', "1"),
    key('2', "2"),
    key('3', "3"),
    key('4', "4"),
    key('5', "5"),
    key('6', "6"),
    key('7', "7"),
    key('8', "8"),
    key('9', "9"),
    key('0', "0"),
    key('-', "-"),
    key('=', "="),
    inert("delete", "Backspace", 1.5),
];

static TOP_ROW: [Cap; 14] = [
    named(KeyCode::Char('\t'), "tab", "Tab", 1.5),
    key('q', "Q"),
    key('w', "W"),
    key('e', "E"),
    key('r', "R"),
    key('t', "T"),
    key('y', "Y"),
    key('u', "U"),
    key('i', "I"),
    key('o', "O"),
    key('p', "P"),
    key('[', "["),
    key(']', "]"),
    key('\\', "\\"),
];

static HOME_ROW: [Cap; 13] = [
    inert("caps lock", "Caps Lock", 1.75),
    key('a', "A"),
    key('s', "S"),
    key('d', "D"),
    key('f', "F"),
    key('g', "G"),
    key('h', "H"),
    key('j', "J"),
    key('k', "K"),
    key('l', "L"),
    key(';', ";"),
    key('\'', "'"),
    named(KeyCode::Char('\r'), "return", "Enter", 1.75),
];

static SHIFT_ROW: [Cap; 12] = [
    modifier(Modifiers::SHIFT, "shift", "Shift", 2.25),
    key('z', "Z"),
    key('x', "X"),
    key('c', "C"),
    key('v', "V"),
    key('b', "B"),
    key('n', "N"),
    key('m', "M"),
    key(',', ","),
    key('.', "."),
    key('/', "/"),
    modifier(Modifiers::SHIFT, "shift", "Shift", 2.25),
];

static MAC_SPACE_ROW: [Cap; 11] = [
    inert("fn", "fn", 1.0),
    modifier(Modifiers::CTRL, "⌃", "Ctrl", 1.0),
    modifier(Modifiers::ALT, "⌥", "Alt", 1.0),
    modifier(Modifiers::SUPER, "⌘", SUPER_LEGEND, 1.25),
    named(KeyCode::Char(' '), "", "", 5.0),
    modifier(Modifiers::SUPER, "⌘", SUPER_LEGEND, 1.25),
    modifier(Modifiers::ALT, "⌥", "Alt", 1.0),
    named(KeyCode::LeftArrow, "←", "←", 1.0),
    arrow(KeyCode::UpArrow, "↑", 1),
    arrow(KeyCode::DownArrow, "↓", 2),
    named(KeyCode::RightArrow, "→", "→", 1.0),
];

static PC_SPACE_ROW: [Cap; 9] = [
    modifier(Modifiers::CTRL, "⌃ control", "Ctrl", 1.25),
    modifier(Modifiers::SUPER, "⌘ command", SUPER_LEGEND, 1.25),
    modifier(Modifiers::ALT, "⌥ option", "Alt", 1.25),
    named(KeyCode::Char(' '), "", "", 6.5),
    modifier(Modifiers::ALT, "⌥ option", "Alt", 1.25),
    named(KeyCode::LeftArrow, "←", "←", 1.0),
    arrow(KeyCode::UpArrow, "↑", 1),
    arrow(KeyCode::DownArrow, "↓", 2),
    named(KeyCode::RightArrow, "→", "→", 1.0),
];

fn rows() -> [&'static [Cap]; 5] {
    let space: &'static [Cap] = if cfg!(target_os = "macos") {
        &MAC_SPACE_ROW
    } else {
        &PC_SPACE_ROW
    };
    [&NUMBER_ROW, &TOP_ROW, &HOME_ROW, &SHIFT_ROW, space]
}

fn cap_at(index: u8) -> Option<&'static Cap> {
    rows().iter().flat_map(|row| row.iter()).nth(index as usize)
}

fn legend(cap: &Cap) -> &'static str {
    if cfg!(target_os = "macos") {
        cap.mac
    } else {
        cap.pc
    }
}

/// The modifier switches above the keyboard, in the platform's order.
fn modifier_chips() -> [(Modifiers, &'static str); 4] {
    if cfg!(target_os = "macos") {
        [
            (Modifiers::SUPER, "⌘ Command"),
            (Modifiers::SHIFT, "⇧ Shift"),
            (Modifiers::ALT, "⌥ Option"),
            (Modifiers::CTRL, "⌃ Control"),
        ]
    } else {
        [
            (Modifiers::CTRL, "Ctrl"),
            (Modifiers::SHIFT, "Shift"),
            (Modifiers::ALT, "Alt"),
            (Modifiers::SUPER, SUPER_LEGEND),
        ]
    }
}

/// Where the platform's own shortcuts start: macOS on ⌘, the others on Ctrl.
fn starting_mods() -> Modifiers {
    if cfg!(target_os = "macos") {
        Modifiers::SUPER
    } else {
        Modifiers::CTRL | Modifiers::SHIFT
    }
}

/// A chord the system takes before ThinkTerm ever sees it.
fn reserved_by_system(key: &KeyCode, mods: Modifiers) -> bool {
    let (super_, shift, ctrl, alt) = (
        Modifiers::SUPER,
        Modifiers::SHIFT,
        Modifiers::CTRL,
        Modifiers::ALT,
    );
    if cfg!(target_os = "macos") {
        match key {
            KeyCode::Char('\t') | KeyCode::Char('`') => mods == super_ || mods == super_ | shift,
            KeyCode::Char(' ') => {
                mods == super_ || mods == ctrl || mods == ctrl | super_ || mods == super_ | alt
            }
            KeyCode::Char('3') | KeyCode::Char('4') | KeyCode::Char('5') => mods == super_ | shift,
            KeyCode::Char('q') => mods == ctrl | super_,
            _ => false,
        }
    } else {
        match key {
            KeyCode::Char('\t') => mods == alt || mods == alt | shift || mods == super_,
            KeyCode::Char('l') | KeyCode::Char('d') | KeyCode::Char('e') | KeyCode::Char('r') => {
                mods == super_
            }
            _ => false,
        }
    }
}

/// A chord the shell and the programs in a terminal use: any key but a
/// function key pressed alone or with Shift -- Enter, the arrows, letters --
/// and a printable key with Ctrl or Alt. Free to bind, but binding it takes
/// it away from them.
fn used_by_programs(key: &KeyCode, mods: Modifiers) -> bool {
    needs_modifier(key, mods)
        || (matches!(key, KeyCode::Char(c) if c.is_ascii_graphic())
            && (mods == Modifiers::CTRL || mods == Modifiers::ALT))
}

/// A key pressed alone or with Shift, other than a function key: it types,
/// moves or edits, so recording one as a shortcut is a slip.
fn needs_modifier(key: &KeyCode, mods: Modifiers) -> bool {
    !matches!(key, KeyCode::Function(_)) && (mods == Modifiers::NONE || mods == Modifiers::SHIFT)
}

/// Where a shortcut comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Default,
    File,
    SetHere,
}

/// What one key of the keyboard does under the chosen modifiers.
#[derive(Debug, Clone, PartialEq)]
enum CapState {
    Free,
    Bound {
        chord: (KeyCode, Modifiers),
        action: KeyAssignment,
        source: Source,
        /// Set here over a different shortcut of the configuration file.
        overrides_file: bool,
    },
    System,
    Programs,
    /// A default or the file's shortcut taken off this key in Settings.
    Removed {
        chord: (KeyCode, Modifiers),
    },
}

/// What the page shows: built when it is entered and after each change.
struct KeymapView {
    /// Every action the palette and the menus offer, with its name.
    actions: Vec<(String, KeyAssignment)>,
    /// The key table as every window has it.
    map: InputMap,
    /// The configuration file's shortcuts and the defaults, without the
    /// layer set here: what a chord falls back to.
    beneath: InputMap,
    file: HashMap<(KeyCode, Modifiers), KeyAssignment>,
    set_here: HashMap<(KeyCode, Modifiers), KeyAssignment>,
    /// A name for every action the key table holds.
    names: HashMap<(KeyCode, Modifiers), String>,
    rendering: UIKeyCapRendering,
}

impl KeymapView {
    fn build() -> Self {
        let config = config::configuration();
        let settings = crate::native_settings::load_shared();
        let layer = crate::native_settings::keymap_entries(&settings);
        let map = InputMap::with_keymap(&config, &layer);
        let beneath = InputMap::with_keymap(&config, &[]);
        let file = config
            .key_bindings()
            .default
            .into_iter()
            .map(|(chord, entry)| (chord, entry.action))
            .collect();
        let set_here = layer
            .into_iter()
            .map(|entry| ((entry.key, entry.mods), entry.action))
            .collect();
        let actions: Vec<(String, KeyAssignment)> = CommandDef::expanded_commands(&config)
            .into_iter()
            .map(|command| (command.brief.to_string(), command.action))
            .collect();
        let names = map
            .keys
            .default
            .iter()
            .map(|(chord, entry)| (chord.clone(), action_name(&actions, &entry.action)))
            .collect();
        Self {
            actions,
            map,
            beneath,
            file,
            set_here,
            names,
            rendering: config.ui_key_cap_rendering,
        }
    }

    /// What `cap` does with `mods` held, its chord looked up in every form a
    /// key event for it may take.
    fn state(&self, cap: &Cap, mods: Modifiers) -> CapState {
        let CapKind::Key(key) = &cap.kind else {
            return CapState::Free;
        };
        if reserved_by_system(key, mods) {
            return CapState::System;
        }
        // Kept in the one form the layer set here writes.
        let chord = crate::inputmap::canonical_chord(key, mods);
        if let Some(action) = self.action_for(&chord) {
            let set_here = self.set_here.contains_key(&chord);
            let file_action = self.file_action(&chord);
            let overrides_file = set_here
                && file_action
                    .as_ref()
                    .is_some_and(|file_action| *file_action != action);
            let source = if set_here {
                Source::SetHere
            } else if file_action.is_some() {
                Source::File
            } else {
                Source::Default
            };
            return CapState::Bound {
                chord,
                action,
                source,
                overrides_file,
            };
        }
        if self.set_here.get(&chord) == Some(&KeyAssignment::DisableDefaultAssignment) {
            return CapState::Removed { chord };
        }
        if used_by_programs(key, mods) {
            CapState::Programs
        } else {
            CapState::Free
        }
    }

    /// What the configuration file binds to `chord`, in any of its forms.
    fn file_action(&self, chord: &(KeyCode, Modifiers)) -> Option<KeyAssignment> {
        crate::inputmap::chord_forms(&chord.0, chord.1)
            .iter()
            .find_map(|form| self.file.get(form))
            .cloned()
    }

    /// What the key table runs for `chord`, in any of its forms.
    fn action_for(&self, chord: &(KeyCode, Modifiers)) -> Option<KeyAssignment> {
        crate::inputmap::chord_forms(&chord.0, chord.1)
            .iter()
            .find_map(|form| self.map.keys.default.get(form))
            .map(|entry| entry.action.clone())
    }

    /// Whether the file or the defaults bind `chord` in any of its forms.
    fn bound_beneath(&self, chord: &(KeyCode, Modifiers)) -> bool {
        crate::inputmap::chord_forms(&chord.0, chord.1)
            .iter()
            .any(|form| self.beneath.keys.default.contains_key(form))
    }

    fn name_of(&self, chord: &(KeyCode, Modifiers), action: &KeyAssignment) -> String {
        self.names
            .get(chord)
            .cloned()
            .unwrap_or_else(|| action_name(&self.actions, action))
    }

    /// The chords that run `action`, as the palette shows them.
    fn chords_for(&self, action: &KeyAssignment) -> Vec<String> {
        let mut chords: Vec<String> = self
            .map
            .keys
            .default
            .iter()
            .filter(|(_, entry)| entry.action == *action)
            .map(|(chord, _)| chord_text(chord, self.rendering))
            .collect();
        chords.sort();
        chords.dedup();
        chords.truncate(2);
        chords
    }
}

fn chord_text(chord: &(KeyCode, Modifiers), rendering: UIKeyCapRendering) -> String {
    let (key, mods) = crate::inputmap::typed_chord(&chord.0, chord.1);
    crate::inputmap::chord_label(&key, mods, rendering)
}

/// A name short enough for a key: the filler words of the commands' names
/// dropped, so "Activate 1st Tab" reads "1st Tab". The detail below the
/// keyboard keeps the whole name.
fn cap_label(name: &str) -> String {
    let words: Vec<&str> = name
        .split(' ')
        .filter(|word| {
            !matches!(
                word.to_ascii_lowercase().as_str(),
                "activate" | "the" | "current" | "currently"
            )
        })
        .collect();
    let mut label = words.join(" ");
    if let Some(first) = label.chars().next() {
        label.replace_range(..first.len_utf8(), &first.to_uppercase().to_string());
    }
    label
}

fn action_name(actions: &[(String, KeyAssignment)], action: &KeyAssignment) -> String {
    if let Some((name, _)) = actions.iter().find(|(_, known)| known == action) {
        return name.clone();
    }
    match crate::commands::derive_command_from_key_assignment(action) {
        Some(command) => command.brief.to_string(),
        None => {
            let mut name = format!("{action:?}");
            if name.len() > 48 {
                name.truncate(name.char_indices().nth(47).map_or(name.len(), |(i, _)| i));
                name.push('…');
            }
            name
        }
    }
}

/// What the page is in the middle of.
#[derive(Debug, Clone, Default)]
enum Mode {
    #[default]
    Idle,
    /// A free key was picked: the list waits for the action to give it.
    Assigning { chord: (KeyCode, Modifiers) },
    /// The next chord pressed goes to `action`, moved off `from` when set.
    Recording {
        action: KeyAssignment,
        name: String,
        from: Option<(KeyCode, Modifiers)>,
    },
    /// A recorded chord another action holds, waiting for Replace or Cancel.
    Confirm {
        action: KeyAssignment,
        chord: (KeyCode, Modifiers),
        from: Option<(KeyCode, Modifiers)>,
        holder: String,
        /// No action holds it, but the shell and its programs use it.
        programs: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeymapAction {
    Modifier(u8),
    Key(u8),
    Change,
    Remove,
    Reset,
    Assign,
    /// A row of the search list, by its place in the list.
    Row(u16),
    Replace,
    Cancel,
    ResetAll,
    ClearSearch,
}

#[derive(Default)]
pub(super) struct KeymapUi {
    pub(super) search: TextInputState,
    mods: Option<Modifiers>,
    selected: Option<u8>,
    view: Option<KeymapView>,
    mode: Mode,
    /// The actions the search list showed, in order, for its row buttons.
    rows: Vec<usize>,
    /// The font the keys' names are set in, with the caption font it was
    /// made beside: a new scale or DPI remakes that, and then this.
    name_font: Option<(Rc<LoadedFont>, Rc<LoadedFont>)>,
}

impl Clone for KeymapUi {
    /// A copy is never recording, and builds its own view when painted.
    fn clone(&self) -> Self {
        Self {
            search: self.search.clone(),
            mods: self.mods,
            selected: self.selected,
            view: None,
            mode: Mode::Idle,
            rows: Vec::new(),
            name_font: None,
        }
    }
}

impl std::fmt::Debug for KeymapUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeymapUi")
            .field("mods", &self.mods)
            .field("selected", &self.selected)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl Drop for KeymapUi {
    fn drop(&mut self) {
        // Settings closing mid-recording must give the menus their keys back.
        if self.is_recording() {
            self.set_mode(Mode::Idle);
        }
    }
}

impl KeymapUi {
    /// The page was entered (`true`) or left.
    pub(super) fn enter(&mut self, entered: bool) {
        self.set_mode(Mode::Idle);
        self.view = entered.then(KeymapView::build);
        if !entered {
            self.rows = Vec::new();
            self.selected = None;
            self.name_font = None;
        }
    }

    /// Rebuild what the page shows, if it is showing.
    pub(super) fn refresh(&mut self) {
        if self.view.is_some() {
            self.view = Some(KeymapView::build());
        }
    }

    pub(super) fn is_recording(&self) -> bool {
        matches!(self.mode, Mode::Recording { .. })
    }

    pub(super) fn stop_recording(&mut self) {
        if self.is_recording() {
            self.set_mode(Mode::Idle);
        }
    }

    fn mods(&self) -> Modifiers {
        self.mods.unwrap_or_else(starting_mods)
    }

    fn set_mode(&mut self, mode: Mode) {
        // On macOS a shortcut being recorded must reach the window rather
        // than run a menu item: ⌘W is recorded, not obeyed.
        #[cfg(target_os = "macos")]
        window::os::macos::set_key_equivalents_captured(matches!(mode, Mode::Recording { .. }));
        self.mode = mode;
    }
}

impl SettingsWindow {
    pub(super) fn paint_keymap(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        if self.ui.keymap.view.is_none() {
            self.ui.keymap.view = Some(KeymapView::build());
        }
        let scroll = self.ui.content_scroll.offset;
        let mut y = self.ui_px(CONTENT_SECTION_Y) - scroll;

        y = self.paint_keymap_toolbar(layers, x, y, max_width)?;
        y = self.paint_keyboard(layers, x, y, max_width)?;
        y += self.ui_px(28.0);

        let searching = !self.ui.keymap.search.text().is_empty()
            || matches!(self.ui.keymap.mode, Mode::Assigning { .. });
        y = match &self.ui.keymap.mode {
            Mode::Recording { .. } | Mode::Confirm { .. } => {
                self.paint_keymap_prompt(layers, x, y, max_width)?
            }
            _ if searching => self.paint_keymap_list(layers, x, y, max_width)?,
            _ => self.paint_keymap_detail(layers, x, y, max_width)?,
        };

        self.ui.content_scroll.set_extents(
            self.content_viewport_extent(),
            self.settings_content_extent(y + scroll),
        );
        Ok(())
    }

    /// The search field and the modifier switches.
    fn paint_keymap_toolbar(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        max_width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let accent = self.chrome_palette.accent;
        let height = self.ui_px(CONTROL_HEIGHT);
        let gap = self.ui_px(12.0);
        let mods = self.ui.keymap.mods();

        // The switches, right-aligned.
        let chips = modifier_chips();
        let pad = self.ui_px(20.0);
        let widths: Vec<f32> = chips
            .iter()
            .map(|(_, label)| self.measure_text_width(&self.ui_font, label) + pad * 2.0)
            .collect();
        let chips_width = widths.iter().sum::<f32>() + gap * (chips.len() as f32 - 1.0);
        let mut chip_x = x + max_width - chips_width;
        for (index, ((bit, label), width)) in chips.iter().zip(widths).enumerate() {
            let action = SettingsAction::Keymap(KeymapAction::Modifier(index as u8));
            let on = mods.contains(*bit);
            let hovered = self.ui.interaction.hovered == Some(action);
            let (fill, border, text) = if on {
                (accent, accent, palette.on_accent)
            } else if hovered {
                (
                    palette.control_hover_bg,
                    palette.control_border,
                    palette.text,
                )
            } else {
                (palette.control_bg, palette.control_border, palette.text)
            };
            // Pills, like the search field beside them.
            self.draw_rounded_frame(
                layers,
                0,
                chip_x,
                y,
                width,
                height,
                fill,
                border,
                height / 2.0,
            )?;
            let text_y = self.control_text_y(y, height);
            self.draw_text(
                layers,
                &Rc::clone(&self.ui_font),
                chip_x + pad,
                text_y,
                label,
                text,
                width,
            )?;
            self.ui_context
                .push(rect(chip_x, y, width, height), WidgetKind::Button, action);
            chip_x += width + gap;
        }

        // The search field takes the rest.
        let field_width = (max_width - chips_width - gap * 2.0).max(self.ui_px(160.0));
        let field = rect(x, y, field_width, height);
        let query = self.ui.keymap.search.text().to_string();
        self.paint_text_input(
            layers,
            0,
            TextInputSpec {
                placeholder: &crate::i18n::tr("settings-keymap-search-placeholder"),
                text: &query,
                rect: field,
                focused: self.ui.interaction.focused == Some(SettingsAction::KeymapSearchInput),
                selected_all: self.ui.keymap.search.selected_all,
                action: SettingsAction::KeymapSearchInput,
            },
        )?;
        let icon_size = self.sidebar_icon_size();
        self.draw_svg_icon(
            layers,
            SettingsIcon::Search.svg(),
            field.origin.x + self.ui_px(15.0),
            field.origin.y + (field.size.height - icon_size) / 2.0,
            icon_size,
            palette.muted_text,
        )?;
        if !query.is_empty() {
            let size = self.ui_px(34.0);
            let clear = rect(
                field.origin.x + field.size.width - size - self.ui_px(10.0),
                field.origin.y + (field.size.height - size) / 2.0,
                size,
                size,
            );
            self.ui_context.push(
                clear,
                WidgetKind::Button,
                SettingsAction::Keymap(KeymapAction::ClearSearch),
            );
            let clear_icon = self.ui_px(24.0);
            self.draw_svg_icon(
                layers,
                SettingsIcon::Clear.svg(),
                clear.origin.x + (size - clear_icon) / 2.0,
                clear.origin.y + (size - clear_icon) / 2.0,
                clear_icon,
                palette.muted_text,
            )?;
        }
        Ok(y + height + self.ui_px(24.0))
    }

    fn paint_keyboard(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        max_width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let accent = self.chrome_palette.accent;
        let danger = self.chrome_palette.danger;
        let mods = self.ui.keymap.mods();
        let selected = self.ui.keymap.selected;
        let unit = max_width / ROW_UNITS;
        let gap = self.ui_px(8.0);
        // Room for the legend and two lines of name below it.
        let cap_height = (unit * 0.95).min(self.ui_px(96.0)).floor();
        let row_step = cap_height + gap;
        let radius = self.ui.tokens.control_radius;
        let legend_font = Rc::clone(&self.logo_caption_font);
        let name_font = self.keymap_name_font()?;
        let legend_metrics = legend_font.metrics();
        // `draw_text` puts every font's baseline where the UI font's falls.
        let baseline_offset =
            self.metrics.cell_size.height as f32 + self.metrics.descender.get() as f32;

        // Work out every key first: painting needs `self` mutably.
        let mut caps = Vec::new();
        {
            let view = self.ui.keymap.view.as_ref().expect("built above");
            let mut index = 0u8;
            for (row, cells) in rows().iter().enumerate() {
                let mut col = 0.0;
                for cap in cells.iter() {
                    let (cy, ch) = match cap.half {
                        1 => (y + row as f32 * row_step, (cap_height - gap) / 2.0),
                        2 => (
                            y + row as f32 * row_step + (cap_height + gap) / 2.0,
                            (cap_height - gap) / 2.0,
                        ),
                        _ => (y + row as f32 * row_step, cap_height),
                    };
                    let cx = x + col * unit;
                    let cw = cap.width * unit - gap;
                    let state = view.state(cap, mods);
                    let name = match &state {
                        CapState::Bound { chord, action, .. } => view.name_of(chord, action),
                        _ => String::new(),
                    };
                    caps.push((index, cap, cx, cy, cw, ch, state, name));
                    index += 1;
                    if cap.half != 1 {
                        col += cap.width;
                    }
                }
            }
        }

        for (index, cap, cx, cy, cw, ch, state, name) in caps {
            let action = match cap.kind {
                CapKind::Modifier(bit) => modifier_chips()
                    .iter()
                    .position(|(chip, _)| *chip == bit)
                    .map(|chip| SettingsAction::Keymap(KeymapAction::Modifier(chip as u8))),
                CapKind::Key(_) => Some(SettingsAction::Keymap(KeymapAction::Key(index))),
                CapKind::Inert => None,
            };
            let hovered = action.is_some() && self.ui.interaction.hovered == action;
            let held = matches!(cap.kind, CapKind::Modifier(bit) if mods.contains(bit));
            let (fill, mut border, text, label_color) = match &state {
                _ if held => (accent, accent, palette.on_accent, palette.on_accent),
                CapState::Bound { overrides_file, .. } => (
                    accent.mul_alpha(0.22),
                    if *overrides_file {
                        danger
                    } else {
                        accent.mul_alpha(0.55)
                    },
                    palette.text,
                    accent,
                ),
                CapState::System | CapState::Programs => (
                    palette.track_off,
                    palette.track_off,
                    palette.muted_text.mul_alpha(0.7),
                    palette.muted_text,
                ),
                CapState::Free | CapState::Removed { .. } => (
                    if hovered {
                        palette.control_hover_bg
                    } else {
                        palette.control_bg
                    },
                    palette.control_border,
                    palette.muted_text,
                    palette.muted_text,
                ),
            };
            if selected == Some(index) && matches!(cap.kind, CapKind::Key(_)) {
                border = palette.text;
            }
            self.draw_rounded_frame(layers, 0, cx, cy, cw, ch, fill, border, radius)?;
            if let Some(action) = action {
                self.ui_context
                    .push(rect(cx, cy, cw, ch), WidgetKind::Button, action);
            }

            let inset = self.ui_px(8.0);
            let legend_text = legend(cap);
            let legend_line = legend_metrics.cell_height.get() as f32;
            let legend_descender = legend_metrics.descender.get() as f32;
            if cap.half != 0 {
                // A half-height arrow: its legend centred, nothing else fits.
                let width = self.measure_text_width(&legend_font, legend_text);
                let baseline = cy + (ch + legend_line) / 2.0 + legend_descender;
                self.draw_text(
                    layers,
                    &legend_font,
                    cx + (cw - width) / 2.0,
                    baseline - baseline_offset,
                    legend_text,
                    text,
                    cw,
                )?;
                continue;
            }
            let legend_baseline = cy + self.ui_px(6.0) + legend_line + legend_descender;
            self.draw_text(
                layers,
                &legend_font,
                cx + inset,
                legend_baseline - baseline_offset,
                legend_text,
                text,
                cw - inset * 2.0,
            )?;
            let shifted = match &cap.kind {
                CapKind::Key(KeyCode::Char(c)) => crate::inputmap::us_shifted(*c),
                _ => None,
            };
            if let Some(shifted) = shifted {
                let shifted = shifted.to_string();
                let width = self.measure_text_width(&name_font, &shifted);
                self.draw_text(
                    layers,
                    &name_font,
                    cx + cw - inset - width,
                    legend_baseline - baseline_offset,
                    &shifted,
                    palette.muted_text,
                    width + 1.0,
                )?;
            }
            let label = match &state {
                CapState::Bound { .. } => cap_label(&name),
                CapState::System => crate::i18n::tr("settings-keymap-cap-system"),
                CapState::Programs => crate::i18n::tr("settings-keymap-cap-programs"),
                CapState::Removed { .. } => crate::i18n::tr("settings-keymap-cap-removed"),
                CapState::Free => String::new(),
            };
            if !label.is_empty() {
                // Up to two lines at the bottom of the key, broken at words.
                let width = cw - inset * 1.2;
                let lines = self.wrap_two_lines(&name_font, &label, width);
                let metrics = name_font.metrics();
                let line_height = metrics.cell_height.get() as f32;
                let mut baseline = cy + ch - self.ui_px(6.0) + metrics.descender.get() as f32;
                for line in lines.iter().rev() {
                    self.draw_text(
                        layers,
                        &name_font,
                        cx + inset,
                        baseline - baseline_offset,
                        line,
                        label_color,
                        width,
                    )?;
                    baseline -= line_height;
                }
            }
        }
        Ok(y + 5.0 * row_step - gap)
    }

    /// The selected key: what it does, and the buttons that change it.
    fn paint_keymap_detail(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        max_width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        let line = self.measure_line_height(&ui_font);
        let mods = self.ui.keymap.mods();
        let selected = self
            .ui
            .keymap
            .selected
            .and_then(cap_at)
            .filter(|cap| matches!(cap.kind, CapKind::Key(_)));

        let Some(cap) = selected else {
            self.draw_text(
                layers,
                &body_font,
                x,
                y,
                &crate::i18n::tr("settings-keymap-hint"),
                palette.muted_text,
                max_width,
            )?;
            return self.paint_keymap_reset_all(layers, x, y + line * 2.0, max_width);
        };
        let CapKind::Key(key) = &cap.kind else {
            unreachable!("filtered above")
        };
        let view = self.ui.keymap.view.as_ref().expect("built before painting");
        let state = view.state(cap, mods);
        let rendering = view.rendering;
        let (chord, title, note) = match &state {
            CapState::Bound {
                chord,
                action,
                source,
                overrides_file,
            } => {
                let source = match source {
                    Source::Default => crate::i18n::tr("settings-keymap-source-default"),
                    Source::File => crate::i18n::tr("settings-keymap-source-file"),
                    Source::SetHere => crate::i18n::tr("settings-keymap-source-set-here"),
                };
                let note = if *overrides_file {
                    let file_action = view.file_action(chord);
                    let name = file_action
                        .map(|action| action_name(&view.actions, &action))
                        .unwrap_or_default();
                    format!(
                        "{source} · {}",
                        settings_tr("settings-keymap-overrides-file", &[("action", name)])
                    )
                } else {
                    source
                };
                (chord.clone(), view.name_of(chord, action), note)
            }
            CapState::Free => (
                crate::inputmap::canonical_chord(key, mods),
                crate::i18n::tr("settings-keymap-free"),
                String::new(),
            ),
            CapState::System => (
                crate::inputmap::canonical_chord(key, mods),
                crate::i18n::tr("settings-keymap-system"),
                String::new(),
            ),
            CapState::Programs => (
                crate::inputmap::canonical_chord(key, mods),
                crate::i18n::tr("settings-keymap-programs"),
                String::new(),
            ),
            CapState::Removed { chord } => (
                chord.clone(),
                crate::i18n::tr("settings-keymap-removed"),
                String::new(),
            ),
        };
        let label = chord_text(&chord, rendering);

        let card_padding = self.ui_px(28.0);
        let card_height = card_padding * 2.0 + line + self.ui_px(CONTROL_HEIGHT) + self.ui_px(20.0);
        self.paint_group_card(layers, x, y, max_width, card_height)?;
        let inner_x = x + card_padding;
        let inner_width = max_width - card_padding * 2.0;
        let mut text_x = inner_x;
        let text_y = y + card_padding;
        let label_width = self.measure_text_width(&ui_font, &label);
        self.draw_text(
            layers,
            &ui_font,
            text_x,
            text_y,
            &label,
            palette.text,
            label_width + 1.0,
        )?;
        text_x += label_width + self.ui_px(24.0);
        let title_width = self.measure_text_width(&ui_font, &title);
        self.draw_text(
            layers,
            &ui_font,
            text_x,
            text_y,
            &title,
            palette.text,
            (inner_x + inner_width - text_x).max(0.0),
        )?;
        text_x += title_width + self.ui_px(20.0);
        if !note.is_empty() {
            self.draw_text(
                layers,
                &body_font,
                text_x,
                text_y,
                &note,
                palette.muted_text,
                (inner_x + inner_width - text_x).max(0.0),
            )?;
        }

        let buttons: Vec<(String, KeymapAction)> = match &state {
            CapState::Bound { source, .. } => {
                let mut buttons = vec![
                    (
                        crate::i18n::tr("settings-keymap-change"),
                        KeymapAction::Change,
                    ),
                    (
                        crate::i18n::tr("settings-keymap-remove"),
                        KeymapAction::Remove,
                    ),
                ];
                if *source == Source::SetHere {
                    buttons.push((
                        crate::i18n::tr("settings-keymap-reset"),
                        KeymapAction::Reset,
                    ));
                }
                buttons
            }
            CapState::Free => vec![(
                crate::i18n::tr("settings-keymap-assign"),
                KeymapAction::Assign,
            )],
            CapState::Programs => vec![(
                crate::i18n::tr("settings-keymap-assign-anyway"),
                KeymapAction::Assign,
            )],
            CapState::Removed { .. } => vec![
                (
                    crate::i18n::tr("settings-keymap-reset"),
                    KeymapAction::Reset,
                ),
                (
                    crate::i18n::tr("settings-keymap-assign"),
                    KeymapAction::Assign,
                ),
            ],
            CapState::System => vec![],
        };
        let mut button_x = inner_x;
        let button_y = text_y + line + self.ui_px(20.0);
        for (label, action) in buttons {
            let width = self.button_width_for_label(&label, 120.0);
            self.draw_button(
                layers,
                button_x,
                button_y,
                width,
                &label,
                SettingsAction::Keymap(action),
            )?;
            button_x += width + self.ui_px(12.0);
        }
        self.paint_keymap_reset_all(layers, x, y + card_height + self.ui_px(28.0), max_width)
    }

    /// Reset All, while anything is set here.
    fn paint_keymap_reset_all(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        _max_width: f32,
    ) -> anyhow::Result<f32> {
        let anything_set = !self.native_settings.keymap.keys.is_empty()
            || self.native_settings.command_palette.hotkey
                != crate::native_settings::NativeCommandPaletteHotkey::default();
        if !anything_set {
            return Ok(y);
        }
        let label = crate::i18n::tr("settings-keymap-reset-all");
        let width = self.button_width_for_label(&label, 120.0);
        self.draw_button(
            layers,
            x,
            y,
            width,
            &label,
            SettingsAction::Keymap(KeymapAction::ResetAll),
        )?;
        Ok(y + self.ui_px(CONTROL_HEIGHT))
    }

    /// Recording, or a recorded chord another action holds.
    fn paint_keymap_prompt(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        max_width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let accent = self.chrome_palette.accent;
        let danger = self.chrome_palette.danger;
        let ui_font = Rc::clone(&self.ui_font);
        let line = self.measure_line_height(&ui_font);
        let rendering = self
            .ui
            .keymap
            .view
            .as_ref()
            .map_or(UIKeyCapRendering::AppleSymbols, |view| view.rendering);
        let (message, border, confirm) = match &self.ui.keymap.mode {
            Mode::Confirm {
                chord,
                programs: true,
                ..
            } => (
                settings_tr(
                    "settings-keymap-taken-programs",
                    &[("chord", chord_text(chord, rendering))],
                ),
                danger,
                Some(crate::i18n::tr("settings-keymap-use-anyway")),
            ),
            Mode::Recording { name, .. } => (
                settings_tr("settings-keymap-recording", &[("action", name.clone())]),
                accent,
                None,
            ),
            Mode::Confirm { chord, holder, .. } => (
                settings_tr(
                    "settings-keymap-taken",
                    &[
                        ("chord", chord_text(chord, rendering)),
                        ("action", holder.clone()),
                    ],
                ),
                danger,
                Some(crate::i18n::tr("settings-keymap-replace")),
            ),
            _ => return Ok(y),
        };
        let padding = self.ui_px(28.0);
        let height = padding * 2.0 + line + self.ui_px(20.0) + self.ui_px(CONTROL_HEIGHT);
        let radius = self.ui.tokens.card_radius;
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            max_width,
            height,
            palette.card_bg,
            border,
            radius,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            x + padding,
            y + padding,
            &message,
            palette.text,
            max_width - padding * 2.0,
        )?;
        let mut button_x = x + padding;
        let button_y = y + padding + line + self.ui_px(20.0);
        if let Some(label) = confirm {
            let width = self.button_width_for_label(&label, 120.0);
            self.draw_button(
                layers,
                button_x,
                button_y,
                width,
                &label,
                SettingsAction::Keymap(KeymapAction::Replace),
            )?;
            button_x += width + self.ui_px(12.0);
        }
        let label = crate::i18n::tr("settings-keymap-cancel");
        let width = self.button_width_for_label(&label, 120.0);
        self.draw_button(
            layers,
            button_x,
            button_y,
            width,
            &label,
            SettingsAction::Keymap(KeymapAction::Cancel),
        )?;
        Ok(y + height)
    }

    /// The actions matching the search, each with its shortcuts and a
    /// button to add one -- or, picking for a free key, to give it that key.
    fn paint_keymap_list(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        mut y: f32,
        max_width: f32,
    ) -> anyhow::Result<f32> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let body_font = Rc::clone(&self.body_font);
        let line = self.measure_line_height(&ui_font);
        let query = self.ui.keymap.search.text().to_lowercase();
        let assigning = match &self.ui.keymap.mode {
            Mode::Assigning { chord } => Some(chord.clone()),
            _ => None,
        };

        // Which actions match, and what each row shows.
        let (rows, total, rendering) = {
            let view = self.ui.keymap.view.as_ref().expect("built before painting");
            let matching: Vec<usize> = view
                .actions
                .iter()
                .enumerate()
                .filter(|(_, (name, _))| query.is_empty() || name.to_lowercase().contains(&query))
                .map(|(index, _)| index)
                .collect();
            let total = matching.len();
            let rows: Vec<(usize, String, Vec<String>)> = matching
                .into_iter()
                .take(LIST_LIMIT)
                .map(|index| {
                    let (name, action) = &view.actions[index];
                    (index, name.clone(), view.chords_for(action))
                })
                .collect();
            (rows, total, view.rendering)
        };
        self.ui.keymap.rows = rows.iter().map(|(index, _, _)| *index).collect();

        if let Some(chord) = &assigning {
            let label = chord_text(chord, rendering);
            let message = settings_tr("settings-keymap-assigning", &[("chord", label)]);
            self.draw_text(
                layers,
                &ui_font,
                x,
                y,
                &message,
                palette.text,
                max_width * 0.7,
            )?;
            let cancel = crate::i18n::tr("settings-keymap-cancel");
            let width = self.button_width_for_label(&cancel, 120.0);
            let button_y = y + (line - self.ui_px(CONTROL_HEIGHT)) / 2.0;
            self.draw_button(
                layers,
                x + max_width - width,
                button_y,
                width,
                &cancel,
                SettingsAction::Keymap(KeymapAction::Cancel),
            )?;
            y += self.ui_px(CONTROL_HEIGHT) + self.ui_px(16.0);
        }

        if rows.is_empty() {
            self.draw_text(
                layers,
                &body_font,
                x,
                y,
                &crate::i18n::tr("settings-keymap-no-results"),
                palette.muted_text,
                max_width,
            )?;
            return Ok(y + line);
        }

        let button_label = if assigning.is_some() {
            crate::i18n::tr("settings-keymap-give")
        } else {
            crate::i18n::tr("settings-keymap-add")
        };
        let button_width = self.button_width_for_label(&button_label, 120.0);
        let row_height = self.ui_px(CONTROL_HEIGHT) + self.ui_px(16.0);
        let padding = self.ui_px(28.0);
        let card_height = row_height * rows.len() as f32 + padding;
        self.paint_group_card(layers, x, y, max_width, card_height)?;
        let mut row_y = y + padding / 2.0;
        for (place, (_, name, chords)) in rows.iter().enumerate() {
            if place > 0 {
                self.draw_rect(
                    layers,
                    0,
                    x + padding,
                    row_y,
                    max_width - padding * 2.0,
                    self.ui_px(1.0),
                    palette.separator,
                )?;
            }
            let text_y = row_y + (row_height - line) / 2.0;
            let button_x = x + max_width - padding - button_width;
            let mut chips_x = button_x - self.ui_px(16.0);
            for chord in chords.iter().rev() {
                let width = self.measure_text_width(&body_font, chord) + self.ui_px(24.0);
                chips_x -= width;
                let chip_height = line + self.ui_px(12.0);
                self.draw_rounded_frame(
                    layers,
                    0,
                    chips_x,
                    text_y - self.ui_px(6.0),
                    width,
                    chip_height,
                    palette.control_bg,
                    palette.control_border,
                    chip_height / 2.0,
                )?;
                self.draw_text(
                    layers,
                    &body_font,
                    chips_x + self.ui_px(12.0),
                    text_y,
                    chord,
                    palette.secondary_text,
                    width,
                )?;
                chips_x -= self.ui_px(8.0);
            }
            self.draw_text(
                layers,
                &ui_font,
                x + padding,
                text_y,
                name,
                palette.text,
                (chips_x - x - padding * 1.5).max(0.0),
            )?;
            let button_y = row_y + (row_height - self.ui_px(CONTROL_HEIGHT)) / 2.0;
            self.draw_button(
                layers,
                button_x,
                button_y,
                button_width,
                &button_label,
                SettingsAction::Keymap(KeymapAction::Row(place as u16)),
            )?;
            row_y += row_height;
        }
        y += card_height;
        if total > rows.len() {
            y += self.ui_px(16.0);
            let more = settings_tr(
                "settings-keymap-more",
                &[("count", (total - rows.len()).to_string())],
            );
            self.draw_text(
                layers,
                &body_font,
                x,
                y,
                &more,
                palette.muted_text,
                max_width,
            )?;
            y += line;
        }
        Ok(y)
    }

    /// The keys' names, all in one size well under the caption font their
    /// legends use.
    fn keymap_name_font(&mut self) -> anyhow::Result<Rc<LoadedFont>> {
        if let Some((caption, font)) = &self.ui.keymap.name_font {
            if Rc::ptr_eq(caption, &self.logo_caption_font) {
                return Ok(Rc::clone(font));
            }
        }
        let font = self.fonts.command_palette_font_with_size_and_weight(
            LOGO_CAPTION_FONT_SIZE * 8.0 / 11.0,
            LOGO_CAPTION_FONT_WEIGHT,
        )?;
        self.ui.keymap.name_font = Some((Rc::clone(&self.logo_caption_font), Rc::clone(&font)));
        Ok(font)
    }

    fn measure_line_height(&self, _font: &Rc<LoadedFont>) -> f32 {
        self.metrics.cell_size.height as f32
    }

    /// `text` in at most two lines of `width`, broken between words; the
    /// second is cut short when it runs on.
    fn wrap_two_lines(&self, font: &Rc<LoadedFont>, text: &str, width: f32) -> Vec<String> {
        if self.measure_text_width(font, text) <= width {
            return vec![text.to_string()];
        }
        let words: Vec<&str> = text.split(' ').collect();
        let mut split = 1;
        while split < words.len()
            && self.measure_text_width(font, &words[..split + 1].join(" ")) <= width
        {
            split += 1;
        }
        let rest = words[split..].join(" ");
        if rest.is_empty() {
            vec![text.to_string()]
        } else {
            vec![words[..split].join(" "), rest]
        }
    }

    pub(super) fn perform_keymap_action(&mut self, action: KeymapAction) {
        match action {
            KeymapAction::Modifier(index) => {
                if let Some((bit, _)) = modifier_chips().get(index as usize) {
                    let mods = self.ui.keymap.mods() ^ *bit;
                    self.ui.keymap.mods = Some(mods);
                }
            }
            KeymapAction::Key(index) => {
                if matches!(
                    self.ui.keymap.mode,
                    Mode::Recording { .. } | Mode::Confirm { .. }
                ) {
                    return;
                }
                self.ui.keymap.selected = Some(index);
                self.ui.keymap.set_mode(Mode::Idle);
            }
            KeymapAction::Change => {
                if let Some((chord, action, name)) = self.keymap_selected_binding() {
                    self.ui.keymap.set_mode(Mode::Recording {
                        action,
                        name,
                        from: Some(chord),
                    });
                }
            }
            KeymapAction::Remove => {
                if let Some((chord, _, _)) = self.keymap_selected_binding() {
                    let beneath = self
                        .ui
                        .keymap
                        .view
                        .as_ref()
                        .is_some_and(|view| view.bound_beneath(&chord));
                    // A chord the file or the defaults bind is freed by
                    // disabling it here; one only this layer bound just goes.
                    let result = crate::native_settings::set_keymap_shortcut(
                        &chord.0,
                        chord.1,
                        beneath.then_some(&KeyAssignment::DisableDefaultAssignment),
                    );
                    self.keymap_saved(result);
                }
            }
            KeymapAction::Reset => {
                if let Some(chord) = self.keymap_selected_chord() {
                    let result =
                        crate::native_settings::set_keymap_shortcut(&chord.0, chord.1, None);
                    self.keymap_saved(result);
                }
            }
            KeymapAction::Assign => {
                if let Some(chord) = self.keymap_selected_chord() {
                    self.ui.keymap.set_mode(Mode::Assigning { chord });
                    self.set_focused_input(Some(SettingsAction::KeymapSearchInput));
                }
            }
            KeymapAction::Row(place) => {
                let Some(index) = self.ui.keymap.rows.get(place as usize).copied() else {
                    return;
                };
                let Some((name, action)) = self
                    .ui
                    .keymap
                    .view
                    .as_ref()
                    .and_then(|view| view.actions.get(index).cloned())
                else {
                    return;
                };
                match std::mem::take(&mut self.ui.keymap.mode) {
                    Mode::Assigning { chord } => self.keymap_bind(action, chord, None),
                    _ => self.ui.keymap.set_mode(Mode::Recording {
                        action,
                        name,
                        from: None,
                    }),
                }
            }
            KeymapAction::Replace => {
                if let Mode::Confirm {
                    action,
                    chord,
                    from,
                    ..
                } = std::mem::take(&mut self.ui.keymap.mode)
                {
                    self.keymap_bind(action, chord, from);
                }
            }
            KeymapAction::Cancel => self.ui.keymap.set_mode(Mode::Idle),
            KeymapAction::ResetAll => {
                let result = crate::native_settings::reset_keymap();
                self.keymap_saved(result);
            }
            KeymapAction::ClearSearch => {
                self.ui.keymap.search.clear();
                self.set_focused_input(Some(SettingsAction::KeymapSearchInput));
            }
        }
    }

    /// A key pressed while a shortcut is being recorded. Always handled:
    /// nothing else may act on it.
    pub(super) fn keymap_record_key(&mut self, event: &KeyEvent) -> bool {
        if !event.key_is_down {
            return true;
        }
        let mods = event.modifiers.remove_positional_mods();
        let key = match &event.key {
            KeyCode::Physical(phys) => phys.to_key_code(),
            other => other.clone(),
        };
        if matches!(
            key,
            KeyCode::Shift
                | KeyCode::LeftShift
                | KeyCode::RightShift
                | KeyCode::Control
                | KeyCode::LeftControl
                | KeyCode::RightControl
                | KeyCode::Alt
                | KeyCode::LeftAlt
                | KeyCode::RightAlt
                | KeyCode::Super
                | KeyCode::LeftWindows
                | KeyCode::RightWindows
                | KeyCode::Hyper
                | KeyCode::Meta
                | KeyCode::CapsLock
                | KeyCode::Composed(_)
        ) {
            return true;
        }
        if key == KeyCode::Char('\u{1b}') && mods == Modifiers::NONE {
            self.ui.keymap.set_mode(Mode::Idle);
            return true;
        }
        if needs_modifier(&key, mods) {
            return true;
        }
        let Mode::Recording { action, from, .. } = self.ui.keymap.mode.clone() else {
            return true;
        };
        let chord = crate::inputmap::canonical_chord(&key, mods);
        let (typed_key, typed_mods) = crate::inputmap::typed_chord(&chord.0, chord.1);
        if reserved_by_system(&typed_key, typed_mods) {
            // It never reaches ThinkTerm; keep listening for another.
            self.status = crate::i18n::tr("settings-keymap-system");
            return true;
        }
        let Some(view) = self.ui.keymap.view.as_ref() else {
            return true;
        };
        let held = view.action_for(&chord);
        if held.as_ref() == Some(&action) {
            // Already this action's: nothing to change.
            self.ui.keymap.set_mode(Mode::Idle);
            return true;
        }
        let holder = held.map(|held| view.name_of(&chord, &held));
        let programs = holder.is_none() && used_by_programs(&typed_key, typed_mods);
        if holder.is_some() || programs {
            self.ui.keymap.set_mode(Mode::Confirm {
                action,
                chord,
                from,
                holder: holder.unwrap_or_default(),
                programs,
            });
        } else {
            self.keymap_bind(action, chord, from);
        }
        true
    }

    /// Give `action` the `chord`, moving it off `from` when set.
    fn keymap_bind(
        &mut self,
        action: KeyAssignment,
        chord: (KeyCode, Modifiers),
        from: Option<(KeyCode, Modifiers)>,
    ) {
        self.ui.keymap.set_mode(Mode::Idle);
        let mut changes = vec![(chord.clone(), Some(action))];
        if let Some(from) = from.filter(|from| *from != chord) {
            // Moving off a chord set here gives it back to what lies
            // beneath; moving off a default or the file's frees it.
            let set_here = self
                .ui
                .keymap
                .view
                .as_ref()
                .is_some_and(|view| view.set_here.contains_key(&from));
            changes.push((
                from,
                (!set_here).then_some(KeyAssignment::DisableDefaultAssignment),
            ));
        }
        // One save, so the move is all or nothing.
        let result = crate::native_settings::set_keymap_shortcuts(&changes);
        self.keymap_saved(result);
    }

    fn keymap_saved(&mut self, result: anyhow::Result<()>) {
        match result {
            Ok(()) => {
                self.set_native_settings(crate::native_settings::load());
                self.ui.keymap.refresh();
                self.status = crate::i18n::tr("settings-status-keymap-saved");
            }
            Err(err) => {
                self.status = settings_tr(
                    "settings-status-keymap-error",
                    &[("error", format!("{err:#}"))],
                );
            }
        }
    }

    fn keymap_selected_chord(&self) -> Option<(KeyCode, Modifiers)> {
        let cap = self.ui.keymap.selected.and_then(cap_at)?;
        let CapKind::Key(key) = &cap.kind else {
            return None;
        };
        Some(crate::inputmap::canonical_chord(key, self.ui.keymap.mods()))
    }

    /// The selected key's shortcut: its chord, action and name.
    fn keymap_selected_binding(&self) -> Option<((KeyCode, Modifiers), KeyAssignment, String)> {
        let cap = self.ui.keymap.selected.and_then(cap_at)?;
        let view = self.ui.keymap.view.as_ref()?;
        match view.state(cap, self.ui.keymap.mods()) {
            CapState::Bound { chord, action, .. } => {
                let name = view.name_of(&chord, &action);
                Some((chord, action, name))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_is_as_wide_as_the_others() {
        for (index, row) in rows().iter().enumerate() {
            let width: f32 = row
                .iter()
                .filter(|cap| cap.half != 2)
                .map(|cap| cap.width)
                .sum();
            assert_eq!(width, ROW_UNITS, "row {index}");
        }
    }

    #[test]
    fn keys_that_type_move_or_edit_need_a_modifier() {
        for key in [
            KeyCode::Char('a'),
            KeyCode::Char('ö'),
            KeyCode::Char('\r'),
            KeyCode::Char('\t'),
            KeyCode::Char('\u{8}'),
            KeyCode::LeftArrow,
        ] {
            assert!(needs_modifier(&key, Modifiers::NONE), "{key:?}");
            assert!(needs_modifier(&key, Modifiers::SHIFT), "{key:?}");
            assert!(!needs_modifier(&key, Modifiers::SUPER), "{key:?}");
        }
        assert!(!needs_modifier(&KeyCode::Function(5), Modifiers::NONE));
        assert!(used_by_programs(&KeyCode::Char('r'), Modifiers::CTRL));
        assert!(used_by_programs(&KeyCode::Char('\r'), Modifiers::NONE));
        assert!(!used_by_programs(&KeyCode::Char('r'), Modifiers::SUPER));
    }

    #[test]
    fn key_names_drop_the_filler_words() {
        assert_eq!(cap_label("Activate 1st Tab"), "1st Tab");
        assert_eq!(cap_label("Close current Pane"), "Close Pane");
        assert_eq!(cap_label("Activate the tab to the right"), "Tab to right");
    }

    /// A view over the defaults with `layer` set here, as the page builds
    /// one, without reading anybody's settings file.
    fn view_with(layer: &[crate::native_settings::KeymapEntry]) -> KeymapView {
        let config = config::ConfigHandle::default_config();
        KeymapView {
            actions: Vec::new(),
            map: InputMap::with_keymap(&config, layer),
            beneath: InputMap::with_keymap(&config, &[]),
            file: HashMap::new(),
            set_here: layer
                .iter()
                .map(|entry| ((entry.key.clone(), entry.mods), entry.action.clone()))
                .collect(),
            names: HashMap::new(),
            rendering: config.ui_key_cap_rendering,
        }
    }

    fn entry(
        key: char,
        mods: Modifiers,
        action: KeyAssignment,
    ) -> crate::native_settings::KeymapEntry {
        let (key, mods) = crate::inputmap::canonical_chord(&KeyCode::Char(key), mods);
        crate::native_settings::KeymapEntry { key, mods, action }
    }

    #[test]
    fn a_removed_default_shows_as_removed_and_can_come_back() {
        let t = &TOP_ROW[5];
        let stock = view_with(&[]);
        assert!(matches!(
            stock.state(t, Modifiers::SUPER),
            CapState::Bound {
                source: Source::Default,
                ..
            }
        ));
        let removed = view_with(&[entry(
            't',
            Modifiers::SUPER,
            KeyAssignment::DisableDefaultAssignment,
        )]);
        assert!(matches!(
            removed.state(t, Modifiers::SUPER),
            CapState::Removed { .. }
        ));
    }

    #[test]
    fn a_shortcut_set_on_a_shifted_symbol_shows_on_its_key() {
        let one = &NUMBER_ROW[1];
        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
        let view = view_with(&[entry('!', Modifiers::CTRL, KeyAssignment::OpenThreadSearch)]);
        match view.state(one, ctrl_shift) {
            CapState::Bound { action, source, .. } => {
                assert_eq!(action, KeyAssignment::OpenThreadSearch);
                assert_eq!(source, Source::SetHere);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_keyboard_fits_in_a_key_index() {
        let caps: usize = rows().iter().map(|row| row.len()).sum();
        assert!(caps <= u8::MAX as usize);
    }
}
