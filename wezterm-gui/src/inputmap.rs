use crate::commands::CommandDef;
use config::keyassignment::{
    ClipboardCopyDestination, ClipboardPasteSource, KeyAssignment, KeyTableEntry, KeyTables,
    MouseEventTrigger, SelectionMode,
};
use config::{ConfigHandle, MouseEventAltScreen, MouseEventTriggerMods};
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;
use wezterm_dynamic::{ToDynamic, Value};
use wezterm_term::input::MouseButton;
use window::{KeyCode, Modifiers, PhysKeyCode, UIKeyCapRendering};

pub struct InputMap {
    pub keys: KeyTables,
    pub mouse: HashMap<(MouseEventTrigger, MouseEventTriggerMods), KeyAssignment>,
    leader: Option<(KeyCode, Modifiers, Duration)>,
}

impl InputMap {
    /// The built-in defaults alone, for `wezterm.gui.default_keys()`: the
    /// shortcuts set in Settings are not defaults, and a configuration that
    /// copies these into its own keys must not copy them.
    pub fn default_input_map() -> Self {
        let config = ConfigHandle::default_config();
        Self::with_keymap(&config, &[])
    }

    pub fn new(config: &ConfigHandle) -> Self {
        let keymap = crate::native_settings::keymap_entries(&crate::native_settings::load_shared());
        Self::with_keymap(config, &keymap)
    }

    /// The input map with `keymap` -- the shortcuts set in Settings → Keymap
    /// -- over the configuration file's keys, which sit over the defaults.
    pub(crate) fn with_keymap(
        config: &ConfigHandle,
        keymap: &[crate::native_settings::KeymapEntry],
    ) -> Self {
        let mut mouse = config.mouse_bindings();

        let mut keys = config.key_bindings();
        for entry in keymap {
            insert_in_every_form(
                &mut keys.default,
                &entry.key,
                entry.mods,
                &entry.action,
                config.key_map_preference,
            );
        }

        let leader = config.leader.as_ref().map(|leader| {
            (
                leader.key.key.resolve(config.key_map_preference).clone(),
                leader.key.mods,
                Duration::from_millis(leader.timeout_milliseconds),
            )
        });

        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;

        macro_rules! m {
            ($([$mod:expr, $code:expr, $action:expr]),* $(,)?) => {
                $(
                mouse.entry(($code, $mod)).or_insert($action);
                )*
            };
        }

        use KeyAssignment::*;

        if !config.disable_default_key_bindings {
            for (mods, code, action) in CommandDef::default_key_assignments(config) {
                // If the user configures {key='p', mods='CTRL|SHIFT'} that gets
                // normalized into {key='P', mods='CTRL'} in Config::key_bindings(),
                // and that value exists in `keys.default` when we reach this point.
                //
                // When we get here with the default assignments for ActivateCommandPalette
                // we are going to register un-normalized entries that don't match
                // the existing normalized entry.
                //
                // Ideally we'd unconditionally normalize_shift
                // here and register the result if it isn't already in the map.
                //
                // Our default set of assignments deliberately and explicitly emits
                // variations on SHIFT as a workaround for an issue with
                // normalization under X11: <https://github.com/wezterm/wezterm/issues/1906>.
                // Until that is resolved, we need to keep emitting both variants.
                //
                // In order for the DisableDefaultAssignment behavior to work with the
                // least surprises, and for these normalization related workarounds
                // to continue? to work, the approach we take here is to lookup the
                // normalized version of what we're about to register, and if we get
                // a match, skip this key.  Otherwise register the non-normalized
                // version from default_key_assignments().
                //
                // See: <https://github.com/wezterm/wezterm/issues/3262>
                let (disable_code, disable_mods) = code.normalize_shift(mods);
                if keys
                    .default
                    .contains_key(&(disable_code.clone(), disable_mods))
                {
                    continue;
                }
                keys.default
                    .entry((code, mods))
                    .or_insert(KeyTableEntry { action });
            }
        }

        // Every ThinkTerm action can be reached from the palette, so with
        // the defaults disabled it keeps its own chords -- unless another
        // chord opens it. Each is kept unless it was bound or freed on
        // purpose: one taken leaves the other.
        if config.disable_default_key_bindings
            && !keys
                .default
                .values()
                .any(|entry| entry.action == KeyAssignment::ActivateCommandPalette)
        {
            for (code, mods) in default_chords(&KeyAssignment::ActivateCommandPalette) {
                let taken = chord_forms(&code, mods)
                    .iter()
                    .any(|form| keys.default.contains_key(form));
                if !taken {
                    insert_in_every_form(
                        &mut keys.default,
                        &code,
                        mods,
                        &KeyAssignment::ActivateCommandPalette,
                        config.key_map_preference,
                    );
                }
            }
        }

        if !config.disable_default_mouse_bindings {
            m!(
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::NONE,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::False,
                    },
                    MouseEventTrigger::Down {
                        streak: 1,
                        button: MouseButton::WheelUp(1),
                    },
                    ScrollByCurrentEventWheelDelta
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::NONE,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::False,
                    },
                    MouseEventTrigger::Down {
                        streak: 1,
                        button: MouseButton::WheelDown(1),
                    },
                    ScrollByCurrentEventWheelDelta
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::NONE,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Down {
                        streak: 3,
                        button: MouseButton::Left
                    },
                    SelectTextAtMouseCursor(SelectionMode::Line)
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::NONE,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Down {
                        streak: 2,
                        button: MouseButton::Left
                    },
                    SelectTextAtMouseCursor(SelectionMode::Word)
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::NONE,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Down {
                        streak: 1,
                        button: MouseButton::Left
                    },
                    SelectTextAtMouseCursor(SelectionMode::Cell)
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::ALT,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Down {
                        streak: 1,
                        button: MouseButton::Left
                    },
                    SelectTextAtMouseCursor(SelectionMode::Block)
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::SHIFT,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Down {
                        streak: 1,
                        button: MouseButton::Left
                    },
                    ExtendSelectionToMouseCursor(SelectionMode::Cell)
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::SHIFT,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Up {
                        streak: 1,
                        button: MouseButton::Left
                    },
                    CompleteSelectionOrOpenLinkAtMouseCursor(
                        ClipboardCopyDestination::ClipboardAndPrimarySelection
                    )
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::NONE,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Up {
                        streak: 1,
                        button: MouseButton::Left
                    },
                    CompleteSelectionOrOpenLinkAtMouseCursor(
                        ClipboardCopyDestination::ClipboardAndPrimarySelection
                    )
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::ALT,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Up {
                        streak: 1,
                        button: MouseButton::Left
                    },
                    CompleteSelection(ClipboardCopyDestination::ClipboardAndPrimarySelection)
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::ALT | Modifiers::SHIFT,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Down {
                        streak: 1,
                        button: MouseButton::Left
                    },
                    ExtendSelectionToMouseCursor(SelectionMode::Block)
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::ALT | Modifiers::SHIFT,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Up {
                        streak: 1,
                        button: MouseButton::Left
                    },
                    CompleteSelectionOrOpenLinkAtMouseCursor(
                        ClipboardCopyDestination::PrimarySelection
                    )
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::NONE,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Up {
                        streak: 2,
                        button: MouseButton::Left
                    },
                    CompleteSelection(ClipboardCopyDestination::ClipboardAndPrimarySelection)
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::NONE,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Up {
                        streak: 3,
                        button: MouseButton::Left
                    },
                    CompleteSelection(ClipboardCopyDestination::ClipboardAndPrimarySelection)
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::NONE,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Drag {
                        streak: 1,
                        button: MouseButton::Left
                    },
                    ExtendSelectionToMouseCursor(SelectionMode::Cell)
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::ALT,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Drag {
                        streak: 1,
                        button: MouseButton::Left
                    },
                    ExtendSelectionToMouseCursor(SelectionMode::Block)
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::NONE,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Drag {
                        streak: 2,
                        button: MouseButton::Left
                    },
                    ExtendSelectionToMouseCursor(SelectionMode::Word)
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::NONE,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Drag {
                        streak: 3,
                        button: MouseButton::Left
                    },
                    ExtendSelectionToMouseCursor(SelectionMode::Line)
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::NONE,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Down {
                        streak: 1,
                        button: MouseButton::Middle
                    },
                    PasteFrom(ClipboardPasteSource::PrimarySelection)
                ],
                [
                    MouseEventTriggerMods {
                        mods: Modifiers::SUPER,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Drag {
                        streak: 1,
                        button: MouseButton::Left,
                    },
                    StartWindowDrag
                ],
                [
                    MouseEventTriggerMods {
                        mods: ctrl_shift,
                        mouse_reporting: false,
                        alt_screen: MouseEventAltScreen::Any,
                    },
                    MouseEventTrigger::Drag {
                        streak: 1,
                        button: MouseButton::Left,
                    },
                    StartWindowDrag
                ],
            );
        }

        keys.default
            .retain(|_, v| v.action != KeyAssignment::DisableDefaultAssignment);

        mouse.retain(|_, v| *v != KeyAssignment::DisableDefaultAssignment);
        // Expand MouseEventAltScreen::Any to individual True/False entries
        let mut expanded_mouse = vec![];
        for ((code, mods), v) in &mouse {
            if mods.alt_screen == MouseEventAltScreen::Any {
                let mods_true = MouseEventTriggerMods {
                    alt_screen: MouseEventAltScreen::True,
                    ..*mods
                };
                let mods_false = MouseEventTriggerMods {
                    alt_screen: MouseEventAltScreen::False,
                    ..*mods
                };
                expanded_mouse.push((code.clone(), mods_true, v.clone()));
                expanded_mouse.push((code.clone(), mods_false, v.clone()));
            }
        }
        // Eliminate ::Any
        mouse.retain(|(_, mods), _| mods.alt_screen != MouseEventAltScreen::Any);
        for (code, mods, v) in expanded_mouse {
            mouse.insert((code, mods), v);
        }

        keys.by_name
            .entry("copy_mode".to_string())
            .or_insert_with(crate::overlay::copy::copy_key_table);
        keys.by_name
            .entry("search_mode".to_string())
            .or_insert_with(crate::overlay::copy::search_key_table);

        Self {
            keys,
            leader,
            mouse,
        }
    }

    /// Given an action, return the corresponding set of application-wide key assignments that are
    /// mapped to it.
    /// If any key_tables reference a given combination, then that combination
    /// is removed from the list.
    /// This is used to figure out whether an application-wide keyboard shortcut
    /// can be safely configured for this action, without interfering with any
    /// transient key_table mappings.
    #[allow(dead_code)]
    pub fn locate_app_wide_key_assignment(
        &self,
        action: &KeyAssignment,
    ) -> Vec<(KeyCode, Modifiers)> {
        let mut candidates = vec![];

        for ((key, mods), entry) in &self.keys.default {
            if mods.contains(Modifiers::LEADER) {
                continue;
            }
            if entry.action == *action {
                candidates.push((key.clone(), mods.clone()));
            }
        }

        // Now ensure that this combination is not part of a key table
        candidates.retain(|tuple| {
            for table in self.keys.by_name.values() {
                if table.contains_key(tuple) {
                    return false;
                }
            }
            true
        });

        candidates
    }

    pub fn is_leader(&self, key: &KeyCode, mods: Modifiers) -> Option<std::time::Duration> {
        if let Some((leader_key, leader_mods, timeout)) = self.leader.as_ref() {
            if *leader_key == *key && *leader_mods == mods.remove_positional_mods() {
                return Some(timeout.clone());
            }
        }
        None
    }

    pub fn has_table(&self, name: &str) -> bool {
        self.keys.by_name.contains_key(name)
    }

    pub fn lookup_key(
        &self,
        key: &KeyCode,
        mods: Modifiers,
        table_name: Option<&str>,
    ) -> Option<KeyTableEntry> {
        let table = match table_name {
            Some(name) => self.keys.by_name.get(name)?,
            None => &self.keys.default,
        };

        table
            .get(&key.normalize_shift(mods.remove_positional_mods()))
            .cloned()
    }

    pub fn lookup_mouse(
        &self,
        event: MouseEventTrigger,
        mut mods: MouseEventTriggerMods,
    ) -> Option<KeyAssignment> {
        mods.mods = mods.mods.remove_positional_mods();
        self.mouse.get(&(event, mods)).cloned()
    }

    pub fn dump_config(&self, key_table: Option<&str>) {
        println!("local wezterm = require 'wezterm'");
        println!("local act = wezterm.action");
        println!();
        println!("return {{");

        if key_table.is_none() {
            println!("  keys = {{");
            show_key_table_as_lua(&self.keys.default, 4);
            println!("  }},");
            println!();
        }

        let mut table_names = self.keys.by_name.keys().collect::<Vec<_>>();
        table_names.sort();
        println!("  key_tables = {{");
        for name in table_names {
            if let Some(wanted_table) = key_table {
                if name != wanted_table {
                    continue;
                }
            }
            if let Some(table) = self.keys.by_name.get(name) {
                println!("    {name} = {{");
                show_key_table_as_lua(table, 6);
                println!("    }},");
                println!();
            }
        }
        println!("  }}");

        println!("}}");
    }

    pub fn show_keys(&self) {
        if let Some((key, mods, duration)) = &self.leader {
            println!("Leader: {key:?} {mods:?} {duration:?}");
        }

        section_header("Default key table");
        show_key_table(&self.keys.default);
        println!();

        let mut table_names = self.keys.by_name.keys().collect::<Vec<_>>();
        table_names.sort();
        for name in table_names {
            if let Some(table) = self.keys.by_name.get(name) {
                section_header(&format!("Key Table: {name}"));
                show_key_table(table);
                println!();
            }
        }

        self.show_mouse();
    }

    fn show_mouse(&self) {
        for (label, alt_screen, mouse_reporting) in [
            ("Mouse", MouseEventAltScreen::False, false),
            ("Mouse: alt_screen", MouseEventAltScreen::True, false),
            ("Mouse: mouse_reporting", MouseEventAltScreen::False, true),
            (
                "Mouse: mouse_reporting + alt_screen",
                MouseEventAltScreen::True,
                true,
            ),
        ] {
            let ordered = self
                .mouse
                .iter()
                .filter(|((_, m), _)| {
                    m.alt_screen == alt_screen && m.mouse_reporting == mouse_reporting
                })
                .collect::<BTreeMap<_, _>>();

            if ordered.is_empty() {
                continue;
            }

            section_header(label);

            let mut trigger_width = 0;
            let mut mod_width = 0;
            for (trigger, mods) in ordered.keys() {
                mod_width = mod_width.max(format!("{:?}", mods.mods).len());
                trigger_width = trigger_width.max(format!("{trigger:?}").len());
            }

            for ((trigger, mods), action) in ordered {
                let mods = if mods.mods == Modifiers::NONE {
                    String::new()
                } else {
                    format!("{:?}", mods.mods)
                };
                let trigger = format!("{trigger:?}");
                println!("\t{mods:mod_width$}   {trigger:trigger_width$}   ->   {action:?}");
            }

            println!();
        }
    }
}

fn section_header(title: &str) {
    let dash = "-".repeat(title.len());
    println!("{title}");
    println!("{dash}");
    println!();
}

/// The shifted symbols of a US layout, by the key that types them.
const US_SHIFTED: [(char, char); 21] = [
    ('`', '~'),
    ('1', '!'),
    ('2', '@'),
    ('3', '#'),
    ('4', '$'),
    ('5', '%'),
    ('6', '^'),
    ('7', '&'),
    ('8', '*'),
    ('9', '('),
    ('0', ')'),
    ('-', '_'),
    ('=', '+'),
    ('[', '{'),
    (']', '}'),
    ('\\', '|'),
    (';', ':'),
    ('\'', '"'),
    (',', '<'),
    ('.', '>'),
    ('/', '?'),
];

/// A chord as it is typed: Shift spelled out, on the key that types it on a
/// US layout. The key table keeps ⌘⇧P as ⌘ with "P"; this is ⌘⇧ with "p".
/// ⌃ with "!" is ⌃⇧ with "1".
pub(crate) fn typed_chord(key: &KeyCode, mods: Modifiers) -> (KeyCode, Modifiers) {
    if let KeyCode::Char(c) = key {
        if c.is_ascii_uppercase() {
            return (
                KeyCode::Char(c.to_ascii_lowercase()),
                mods | Modifiers::SHIFT,
            );
        }
        if let Some((base, _)) = US_SHIFTED.iter().find(|(_, shifted)| shifted == c) {
            return (KeyCode::Char(*base), mods | Modifiers::SHIFT);
        }
    }
    (key.clone(), mods)
}

/// The one form Settings → Keymap keeps a chord in and compares chords by:
/// as typed, then normalized as the key table normalizes.
pub(crate) fn canonical_chord(key: &KeyCode, mods: Modifiers) -> (KeyCode, Modifiers) {
    let (key, mods) = typed_chord(key, mods.remove_positional_mods());
    key.normalize_shift(mods)
}

/// The chord a key press records in Settings → Keymap: `canonical_chord`,
/// which names a symbol by the key that types it on a US layout, as the
/// drawn keyboard and the defaults name it -- unless the keyboard pressed
/// types some other symbol on that key. Then the US name would be another
/// key's: ⇧7 types "/" on a German layout and ⇧ß types "?", which a US
/// layout types with ⇧/, so the two keys would be one shortcut, and each
/// would take the other's spellings. Such a chord is kept as the physical
/// key pressed, which no other key shares and which is looked up first.
pub(crate) fn recorded_chord(
    phys: Option<PhysKeyCode>,
    key: &KeyCode,
    mods: Modifiers,
) -> (KeyCode, Modifiers) {
    if let (Some(phys), KeyCode::Char(c)) = (phys, key) {
        let symbol = !c.is_ascii_alphanumeric() && !c.is_control() && *c != ' ';
        if symbol {
            if let KeyCode::Char(base) = phys.to_key_code() {
                if *c != base && us_shifted(base) != Some(*c) {
                    return (KeyCode::Physical(phys), mods.remove_positional_mods());
                }
            }
        }
    }
    canonical_chord(key, mods)
}

/// The key that types `c` with Shift on a US layout: "!" for "1".
pub(crate) fn us_shifted(c: char) -> Option<char> {
    US_SHIFTED
        .iter()
        .find(|(base, _)| *base == c)
        .map(|(_, shifted)| *shifted)
}

/// Every form the key table may be asked for a chord in, in the order a key
/// press looks them up: the physical key, whose bindings are tried first;
/// the canonical one; and for a shifted symbol, the symbol with and without
/// Shift, as platforms report it either way.
pub(crate) fn chord_forms(key: &KeyCode, mods: Modifiers) -> Vec<(KeyCode, Modifiers)> {
    let canonical = canonical_chord(key, mods);
    let (typed_key, typed_mods) = typed_chord(&canonical.0, canonical.1);
    let mut forms = vec![];
    if let Some(phys) = typed_key.to_phys() {
        let physical = (KeyCode::Physical(phys), typed_mods);
        if physical != canonical {
            forms.push(physical);
        }
    }
    forms.push(canonical);
    if let KeyCode::Char(c) = typed_key {
        if typed_mods.contains(Modifiers::SHIFT) {
            if let Some(shifted) = us_shifted(c) {
                forms.push((KeyCode::Char(shifted), typed_mods));
                forms.push((KeyCode::Char(shifted), typed_mods - Modifiers::SHIFT));
            }
        }
    }
    forms
}

/// Bind `action` in `table` in every form a key event for the chord may be
/// looked up in, so it wins over the shifted-symbol spellings of the same
/// keys. The physical form of a typed chord is taken only where a physical
/// binding would otherwise win: under `key_map_preference = "Physical"`, or
/// where the table already binds it. Elsewhere it would take a key by its
/// US position on a layout that types something else there. A chord that
/// is a physical key (`recorded_chord`) is bound as one.
fn insert_in_every_form(
    table: &mut config::keyassignment::KeyTable,
    key: &KeyCode,
    mods: Modifiers,
    action: &KeyAssignment,
    preference: config::KeyMapPreference,
) {
    let chord_is_physical = matches!(key, KeyCode::Physical(_));
    for form in chord_forms(key, mods) {
        let physical = matches!(form.0, KeyCode::Physical(_));
        if physical
            && !chord_is_physical
            && preference != config::KeyMapPreference::Physical
            && !table.contains_key(&form)
        {
            continue;
        }
        table.insert(
            form,
            KeyTableEntry {
                action: action.clone(),
            },
        );
    }
}

/// The chords `action` is bound to by default, as its definition writes
/// them, one canonical form each.
pub(crate) fn default_chords(action: &KeyAssignment) -> Vec<(KeyCode, Modifiers)> {
    use std::convert::TryFrom;
    crate::commands::derive_command_from_key_assignment(action)
        .map(|def| {
            def.keys
                .iter()
                .filter_map(|(mods, label)| {
                    let key = config::DeferredKeyCode::try_from(label.as_str())
                        .ok()?
                        .resolve(config::KeyMapPreference::Mapped);
                    Some(canonical_chord(&key, *mods))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A chord as the palette and Settings → Keymap show it: `⌘ ⇧ P` under
/// AppleSymbols, `CTRL-SHIFT-P` otherwise.
pub(crate) fn chord_label(
    key: &KeyCode,
    mods: Modifiers,
    ui_key_cap_rendering: UIKeyCapRendering,
) -> String {
    let separator = if ui_key_cap_rendering == UIKeyCapRendering::AppleSymbols {
        " "
    } else {
        "-"
    };
    let mut label = mods.to_string_with_separator(window::ModifierToStringArgs {
        separator,
        want_none: false,
        ui_key_cap_rendering: Some(ui_key_cap_rendering),
    });
    if !label.is_empty() {
        label.push_str(separator);
    }
    label.push_str(&ui_key(key, ui_key_cap_rendering));
    label
}

/// `key` as the configuration file spells it: parsing the result gives
/// `key` back. Settings → Keymap writes its shortcuts with it.
pub(crate) fn config_key_name(key: &KeyCode) -> String {
    match key {
        KeyCode::Char('\u{8}') => "Backspace".to_string(),
        KeyCode::Char('\t') => "Tab".to_string(),
        KeyCode::Char('\r') => "Enter".to_string(),
        KeyCode::Char('\u{1b}') => "Escape".to_string(),
        KeyCode::Char('\u{7f}') => "Delete".to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Function(n) => format!("F{n}"),
        KeyCode::Numpad(n) => format!("Numpad{n}"),
        KeyCode::Physical(phys) => format!("phys:{}", phys.to_string()),
        KeyCode::RawCode(n) => format!("raw:{n}"),
        // The named keys parse from their variant names: LeftArrow, Home...
        other => format!("{other:?}"),
    }
}

pub fn ui_key(key: &KeyCode, ui_key_cap_rendering: UIKeyCapRendering) -> String {
    match key {
        KeyCode::Char('\x1b') | KeyCode::Char('\x7f')
            if ui_key_cap_rendering == UIKeyCapRendering::AppleSymbols =>
        {
            "\u{238b}".to_string()
        }
        KeyCode::Char('\x1b') | KeyCode::Char('\x7f') => "Esc".to_string(),
        KeyCode::Char('\x08') if ui_key_cap_rendering == UIKeyCapRendering::AppleSymbols => {
            "\u{232b}".to_string()
        }
        KeyCode::Char('\x08') => "Del".to_string(),
        KeyCode::Char('\r') if ui_key_cap_rendering == UIKeyCapRendering::AppleSymbols => {
            "\u{21b5}".to_string()
        }
        KeyCode::Char('\r') => "Enter".to_string(),
        KeyCode::Physical(PhysKeyCode::Space) | KeyCode::Char(' ')
            if ui_key_cap_rendering == UIKeyCapRendering::AppleSymbols =>
        {
            "\u{2423}".to_string()
        }
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char('\t') if ui_key_cap_rendering == UIKeyCapRendering::AppleSymbols => {
            "\u{21e5}".to_string()
        }
        KeyCode::Char('\t') => "Tab".to_string(),
        KeyCode::Char(c) if c.is_ascii_control() => c.escape_debug().to_string(),
        KeyCode::Char(c) => c.to_uppercase().to_string(),

        KeyCode::Physical(PhysKeyCode::PageUp) | KeyCode::PageUp
            if ui_key_cap_rendering == UIKeyCapRendering::AppleSymbols =>
        {
            "\u{21de}".to_string()
        }
        KeyCode::Physical(PhysKeyCode::PageDown) | KeyCode::PageDown
            if ui_key_cap_rendering == UIKeyCapRendering::AppleSymbols =>
        {
            "\u{21df}".to_string()
        }
        KeyCode::Physical(PhysKeyCode::LeftArrow) | KeyCode::LeftArrow => "\u{2190}".to_string(),
        KeyCode::Physical(PhysKeyCode::UpArrow) | KeyCode::UpArrow => "\u{2191}".to_string(),
        KeyCode::Physical(PhysKeyCode::RightArrow) | KeyCode::RightArrow => "\u{2192}".to_string(),
        KeyCode::Physical(PhysKeyCode::DownArrow) | KeyCode::DownArrow => "\u{2193}".to_string(),
        KeyCode::Function(n) => format!("F{n}"),
        KeyCode::Numpad(n) => format!("Numpad{n}"),
        KeyCode::Physical(phys) => phys.to_string(),
        _ => format!("{key:?}"),
    }
}

pub fn human_key(key: &KeyCode) -> String {
    match key {
        KeyCode::Char('\x1b') => "Escape".to_string(),
        KeyCode::Char('\x7f') => "Escape".to_string(),
        KeyCode::Char('\x08') => "Backspace".to_string(),
        KeyCode::Char('\r') => "Enter".to_string(),
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char('\t') => "Tab".to_string(),
        KeyCode::Char(c) if c.is_ascii_control() => c.escape_debug().to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Function(n) => format!("F{n}"),
        KeyCode::Numpad(n) => format!("Numpad{n}"),
        KeyCode::Physical(phys) => format!("{} (Physical)", phys.to_string()),
        _ => format!("{key:?}"),
    }
}

fn lua_key_code(key: &KeyCode) -> String {
    match key {
        KeyCode::Char('\x1b') => "Escape".to_string(),
        KeyCode::Char('\x7f') => "Escape".to_string(),
        KeyCode::Char('\x08') => "Backspace".to_string(),
        KeyCode::Char('\r') => "Enter".to_string(),
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char('\t') => "Tab".to_string(),
        KeyCode::Char(c) if c.is_ascii_control() => c.escape_debug().to_string(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Function(n) => format!("F{n}"),
        KeyCode::Numpad(n) => format!("Numpad{n}"),
        KeyCode::Physical(phys) => format!("phys:{}", phys.to_string()),
        _ => format!("{key:?}"),
    }
}

fn luaify(value: Value, is_top: bool) -> String {
    match value {
        Value::String(s) if is_top => format!("act.{s}"),
        Value::String(s) => quote_lua_string(&s),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        Value::Null => "nil".to_string(),
        Value::U64(u) => u.to_string(),
        Value::F64(u) => u.to_string(),
        Value::I64(u) => u.to_string(),
        Value::Array(a) => {
            format!("wat {a:?}")
        }
        Value::Object(o) if is_top => {
            for (k, v) in o {
                let k = match k {
                    Value::String(s) => s,
                    _ => unreachable!(),
                };
                let arg = match v {
                    Value::String(_) => format!(" {}", luaify(v, false)),
                    Value::Array(a) => {
                        let b: Vec<String> = a.into_iter().map(|v| luaify(v, false)).collect();
                        format!("{{ {} }}", b.join(", "))
                    }
                    Value::I64(i) => format!("({i})"),
                    Value::U64(i) => format!("({i})"),
                    Value::F64(i) => format!("({i})"),
                    _ => luaify(v, false),
                };
                return format!("act.{k}{arg}");
            }
            unreachable!()
        }
        Value::Object(o) => {
            let mut fields = vec![];
            for (k, v) in o {
                let k = match k {
                    Value::String(s) => s,
                    _ => unreachable!(),
                };
                let arg = match v {
                    Value::Null => continue,
                    Value::String(_) => format!(" {}", luaify(v, false)),
                    Value::Array(a) => {
                        let b: Vec<String> = a.into_iter().map(|v| luaify(v, false)).collect();
                        format!("{{ {} }}", b.join(", "))
                    }
                    Value::I64(i) => format!("({i})"),
                    Value::U64(i) => format!("({i})"),
                    Value::F64(i) => format!("({i})"),
                    Value::Object(o) if o.is_empty() => continue,
                    _ => luaify(v, false),
                };
                fields.push(format!("{k} = {arg}"));
            }
            format!("{{ {} }}", fields.join(", "))
        }
    }
}

fn quote_lua_string(s: &str) -> String {
    let mut result = String::new();
    result.push('\'');
    for c in s.chars() {
        match c {
            '\u{07}' => {
                result.push_str("\\a");
            }
            '\u{08}' => {
                result.push_str("\\b");
            }
            '\u{0c}' => {
                result.push_str("\\f");
            }
            '\n' => {
                result.push_str("\\n");
            }
            '\r' => {
                result.push_str("\\r");
            }
            '\t' => {
                result.push_str("\\t");
            }
            '\u{0b}' => {
                result.push_str("\\v");
            }
            '\\' => {
                result.push_str("\\\\");
            }
            '"' => {
                result.push_str("\\\"");
            }
            '\'' => {
                result.push_str("\\'");
            }
            c if c.is_alphanumeric() || c.is_ascii_punctuation() => {
                result.push(c);
            }
            _ => {
                let b = c as u32;
                result.push_str(&format!("\\u{{{b:x}}}"));
            }
        }
    }
    result.push('\'');
    result
}

fn lua_key(key: &KeyCode, mods: Modifiers, action: &KeyAssignment) -> String {
    let dyn_action = action.to_dynamic();
    // println!(" -- {dyn_action:?}");
    let action = luaify(dyn_action, true);
    let key = lua_key_code(key);
    let key = quote_lua_string(&key);

    let mods = format!("{mods:?}").replace(" ", "");

    format!("{{ key = {key}, mods = '{mods}', action = {action} }}")
}

fn show_key_table(table: &config::keyassignment::KeyTable) {
    let ordered = table.iter().collect::<BTreeMap<_, _>>();

    let mut key_width = 0;
    let mut mod_width = 0;
    for (key, mods) in ordered.keys() {
        mod_width = mod_width.max(format!("{mods:?}").len());
        key_width = key_width.max(human_key(key).len());
    }

    for ((key, mods), entry) in ordered {
        let action = &entry.action;
        let mods = if *mods == Modifiers::NONE {
            String::new()
        } else {
            format!("{mods:?}")
        };
        let key = human_key(key);
        println!("\t{mods:mod_width$}   {key:key_width$}   ->   {action:?}");
    }
}

fn show_key_table_as_lua(table: &config::keyassignment::KeyTable, indent: usize) {
    let ordered = table.iter().collect::<BTreeMap<_, _>>();

    let pad = " ".repeat(indent);
    for ((key, mods), entry) in ordered {
        let action = &entry.action;
        println!("{pad}{},", lua_key(key, *mods, action));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_settings::KeymapEntry;

    fn entry(key: char, mods: Modifiers, action: KeyAssignment) -> KeymapEntry {
        KeymapEntry {
            key: KeyCode::Char(key),
            mods,
            action,
        }
    }

    fn action_for(map: &InputMap, key: char, mods: Modifiers) -> Option<KeyAssignment> {
        map.keys
            .default
            .get(&(KeyCode::Char(key), mods))
            .map(|entry| entry.action.clone())
    }

    #[test]
    fn the_keymap_layer_wins_over_the_defaults() {
        let config = ConfigHandle::default_config();
        let stock = InputMap::with_keymap(&config, &[]);
        assert_ne!(
            action_for(&stock, 'k', Modifiers::SUPER),
            Some(KeyAssignment::ActivateCommandPalette)
        );

        let map = InputMap::with_keymap(
            &config,
            &[entry(
                'k',
                Modifiers::SUPER,
                KeyAssignment::ActivateCommandPalette,
            )],
        );
        assert_eq!(
            action_for(&map, 'k', Modifiers::SUPER),
            Some(KeyAssignment::ActivateCommandPalette)
        );
    }

    #[test]
    fn disabling_a_default_frees_its_key() {
        let config = ConfigHandle::default_config();
        assert!(action_for(&InputMap::with_keymap(&config, &[]), 't', Modifiers::SUPER).is_some());
        let map = InputMap::with_keymap(
            &config,
            &[entry(
                't',
                Modifiers::SUPER,
                KeyAssignment::DisableDefaultAssignment,
            )],
        );
        assert_eq!(action_for(&map, 't', Modifiers::SUPER), None);
    }

    #[test]
    fn the_palette_stays_reachable_with_the_defaults_disabled() {
        let config = ConfigHandle::default_config()
            .adjusted(|config| config.disable_default_key_bindings = true);
        let map = InputMap::with_keymap(&config, &[]);
        assert!(map
            .keys
            .default
            .values()
            .any(|entry| entry.action == KeyAssignment::ActivateCommandPalette));
    }

    #[test]
    fn a_freed_palette_chord_stays_free() {
        let primary = if cfg!(target_os = "macos") {
            Modifiers::SUPER
        } else {
            Modifiers::CTRL
        };
        let (key, mods) = KeyCode::Char('p').normalize_shift(primary | Modifiers::SHIFT);
        let map = InputMap::with_keymap(
            &ConfigHandle::default_config(),
            &[KeymapEntry {
                key: key.clone(),
                mods,
                action: KeyAssignment::DisableDefaultAssignment,
            }],
        );
        // No form of the chord opens the palette, so no menu offers it.
        assert!(!map.keys.default.iter().any(|((code, m), entry)| {
            entry.action == KeyAssignment::ActivateCommandPalette
                && code.normalize_shift(*m) == (key.clone(), mods)
        }));
    }

    #[test]
    fn the_keymap_layer_reads_typed_keys_under_the_physical_preference() {
        let config = ConfigHandle::default_config()
            .adjusted(|config| config.key_map_preference = config::KeyMapPreference::Physical);
        let map = InputMap::with_keymap(
            &config,
            &[entry(
                'j',
                Modifiers::SUPER,
                KeyAssignment::OpenThreadSearch,
            )],
        );
        assert_eq!(
            map.lookup_key(&KeyCode::Char('j'), Modifiers::SUPER, None)
                .map(|entry| entry.action),
            Some(KeyAssignment::OpenThreadSearch)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn find_thread_takes_cmd_p_and_leaves_ctrl_shift_p_to_the_palette() {
        let map = InputMap::with_keymap(&ConfigHandle::default_config(), &[]);
        assert_eq!(
            action_for(&map, 'p', Modifiers::SUPER),
            Some(KeyAssignment::OpenThreadSearch)
        );
        assert_eq!(
            action_for(&map, 'P', Modifiers::CTRL),
            Some(KeyAssignment::ActivateCommandPalette)
        );
    }

    #[test]
    fn a_shifted_symbol_shortcut_covers_every_spelling() {
        let config = ConfigHandle::default_config();
        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
        let spellings = [
            (KeyCode::Char('1'), ctrl_shift),
            (KeyCode::Char('!'), ctrl_shift),
            (KeyCode::Char('!'), Modifiers::CTRL),
        ];
        let stock = InputMap::with_keymap(&config, &[]);
        for (key, mods) in &spellings {
            assert!(
                stock.keys.default.contains_key(&(key.clone(), *mods)),
                "{key:?}"
            );
        }

        let (key, mods) = canonical_chord(&KeyCode::Char('!'), Modifiers::CTRL);
        let freed = InputMap::with_keymap(
            &config,
            &[KeymapEntry {
                key,
                mods,
                action: KeyAssignment::DisableDefaultAssignment,
            }],
        );
        for (key, mods) in &spellings {
            assert!(
                !freed.keys.default.contains_key(&(key.clone(), *mods)),
                "{key:?}"
            );
        }
    }

    #[test]
    fn the_keymap_layer_wins_over_physical_bindings() {
        let config = ConfigHandle::default_config()
            .adjusted(|config| config.key_map_preference = config::KeyMapPreference::Physical);
        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
        let (key, mods) = canonical_chord(&KeyCode::Char('c'), ctrl_shift);
        let map = InputMap::with_keymap(
            &config,
            &[KeymapEntry {
                key,
                mods,
                action: KeyAssignment::ToggleWorkspaceSidebar,
            }],
        );
        assert_eq!(
            map.lookup_key(&KeyCode::Physical(PhysKeyCode::C), ctrl_shift, None)
                .map(|entry| entry.action),
            Some(KeyAssignment::ToggleWorkspaceSidebar)
        );
    }

    #[test]
    fn the_keymap_layer_wins_over_a_physical_binding_in_the_file() {
        let config = ConfigHandle::default_config();
        let mut map = InputMap::with_keymap(&config, &[]);
        assert!(!map
            .keys
            .default
            .contains_key(&(KeyCode::Physical(PhysKeyCode::K), Modifiers::SUPER)));
        // As a `phys:K` binding in the file would put it.
        map.keys.default.insert(
            (KeyCode::Physical(PhysKeyCode::K), Modifiers::SUPER),
            KeyTableEntry {
                action: KeyAssignment::ClearScrollback(
                    config::keyassignment::ScrollbackEraseMode::ScrollbackOnly,
                ),
            },
        );
        insert_in_every_form(
            &mut map.keys.default,
            &KeyCode::Char('k'),
            Modifiers::SUPER,
            &KeyAssignment::ActivateCommandPalette,
            config.key_map_preference,
        );
        assert_eq!(
            map.lookup_key(&KeyCode::Physical(PhysKeyCode::K), Modifiers::SUPER, None)
                .map(|entry| entry.action),
            Some(KeyAssignment::ActivateCommandPalette)
        );
        // And no physical form is taken where nothing bound one.
        insert_in_every_form(
            &mut map.keys.default,
            &KeyCode::Char('j'),
            Modifiers::SUPER,
            &KeyAssignment::OpenThreadSearch,
            config.key_map_preference,
        );
        assert!(!map
            .keys
            .default
            .contains_key(&(KeyCode::Physical(PhysKeyCode::J), Modifiers::SUPER)));
    }

    #[test]
    fn chords_read_as_typed() {
        assert_eq!(
            typed_chord(&KeyCode::Char('P'), Modifiers::SUPER),
            (KeyCode::Char('p'), Modifiers::SUPER | Modifiers::SHIFT)
        );
        assert_eq!(
            typed_chord(&KeyCode::Char('!'), Modifiers::CTRL),
            (KeyCode::Char('1'), Modifiers::CTRL | Modifiers::SHIFT)
        );
        assert_eq!(
            canonical_chord(&KeyCode::Char('p'), Modifiers::SUPER | Modifiers::SHIFT),
            (KeyCode::Char('P'), Modifiers::SUPER)
        );
    }

    #[test]
    fn another_layouts_symbol_is_kept_as_the_key_pressed() {
        let shift_cmd = Modifiers::SUPER | Modifiers::SHIFT;
        // A US layout: the symbol is named by the key that types it.
        for typed in ['?', '/'] {
            assert_eq!(
                recorded_chord(Some(PhysKeyCode::Slash), &KeyCode::Char(typed), shift_cmd),
                canonical_chord(&KeyCode::Char(typed), shift_cmd)
            );
        }
        // German: ⇧7 types "/" and ⇧ß types "?". Two keys, two shortcuts,
        // neither of them ⇧/.
        let seven = recorded_chord(Some(PhysKeyCode::K7), &KeyCode::Char('/'), shift_cmd);
        let eszett = recorded_chord(Some(PhysKeyCode::Minus), &KeyCode::Char('?'), shift_cmd);
        assert_eq!(seven, (KeyCode::Physical(PhysKeyCode::K7), shift_cmd));
        assert_eq!(eszett, (KeyCode::Physical(PhysKeyCode::Minus), shift_cmd));
        assert_ne!(seven, canonical_chord(&KeyCode::Char('?'), shift_cmd));
        // Letters and digits keep their names, as the drawn keyboard has them.
        assert_eq!(
            recorded_chord(Some(PhysKeyCode::Q), &KeyCode::Char('a'), Modifiers::SUPER),
            canonical_chord(&KeyCode::Char('a'), Modifiers::SUPER)
        );
        // Without a physical key, as before.
        assert_eq!(
            recorded_chord(None, &KeyCode::Char('?'), shift_cmd),
            canonical_chord(&KeyCode::Char('?'), shift_cmd)
        );

        // Bound as the physical key, and only as it.
        let map = InputMap::with_keymap(
            &ConfigHandle::default_config(),
            &[KeymapEntry {
                key: seven.0.clone(),
                mods: seven.1,
                action: KeyAssignment::OpenThreadSearch,
            }],
        );
        assert_eq!(
            map.lookup_key(&seven.0, seven.1, None).map(|entry| entry.action),
            Some(KeyAssignment::OpenThreadSearch)
        );
        assert_ne!(
            map.lookup_key(&KeyCode::Char('?'), shift_cmd, None)
                .map(|entry| entry.action),
            Some(KeyAssignment::OpenThreadSearch)
        );
    }

    #[test]
    fn a_default_palette_chord_taken_leaves_the_other() {
        let config = ConfigHandle::default_config()
            .adjusted(|config| config.disable_default_key_bindings = true);
        let chords = default_chords(&KeyAssignment::ActivateCommandPalette);
        if chords.len() < 2 {
            return;
        }
        let (taken, kept) = (&chords[0], &chords[1]);
        let map = InputMap::with_keymap(
            &config,
            &[KeymapEntry {
                key: taken.0.clone(),
                mods: taken.1,
                action: KeyAssignment::OpenThreadSearch,
            }],
        );
        assert_eq!(
            map.lookup_key(&kept.0, kept.1, None).map(|entry| entry.action),
            Some(KeyAssignment::ActivateCommandPalette)
        );
        assert_eq!(
            map.lookup_key(&taken.0, taken.1, None).map(|entry| entry.action),
            Some(KeyAssignment::OpenThreadSearch)
        );
    }

    #[test]
    fn config_key_names_parse_back_to_their_key() {
        use std::convert::TryFrom;
        for key in [
            KeyCode::Char('p'),
            KeyCode::Char('P'),
            KeyCode::Char(','),
            KeyCode::Char('\\'),
            KeyCode::Char(' '),
            KeyCode::Char('\r'),
            KeyCode::Char('\t'),
            KeyCode::Char('\u{8}'),
            KeyCode::Char('\u{1b}'),
            KeyCode::Char('\u{7f}'),
            KeyCode::Function(5),
            KeyCode::LeftArrow,
            KeyCode::PageUp,
            KeyCode::Home,
            KeyCode::KeyPadHome,
            KeyCode::KeyPadEnd,
            KeyCode::KeyPadPageUp,
            KeyCode::KeyPadPageDown,
            KeyCode::KeyPadBegin,
            KeyCode::Physical(PhysKeyCode::A),
            KeyCode::Physical(PhysKeyCode::K7),
        ] {
            let name = config_key_name(&key);
            let parsed = config::DeferredKeyCode::try_from(name.as_str())
                .unwrap()
                .resolve(config::KeyMapPreference::Mapped);
            assert_eq!(parsed, key, "{name:?}");
        }
    }
}
