//! Data layer for the command palette: command enumeration, recents/frecency
//! tracking and shortcut formatting. The UI lives in
//! `termwindow/ui/command_palette.rs`.

use crate::commands::{CommandDef, ExpandedCommand};
use crate::inputmap::InputMap;
use crate::termwindow::GuiWin;
use config::keyassignment::KeyAssignment;
use config::ConfigHandle;
use frecency::Frecency;
use luahelper::{from_lua_value_dynamic, impl_lua_conversion_dynamic};
use mux_lua::MuxPane;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::PathBuf;
use wezterm_dynamic::{FromDynamic, ToDynamic};
use window::Modifiers;

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Recent {
    brief: String,
    frecency: Frecency,
}

fn recent_file_name() -> PathBuf {
    config::DATA_DIR.join("recent-commands.json")
}

fn load_recents() -> anyhow::Result<Vec<Recent>> {
    let file_name = recent_file_name();
    let text = std::fs::read_to_string(&file_name)?;
    let mut recents: Vec<Recent> = serde_json::from_str(&text)?;
    recents.sort_by(|a, b| b.frecency.score().partial_cmp(&a.frecency.score()).unwrap());
    Ok(recents)
}

/// Cap on stored recents. The palette now records every activation —
/// including individual color schemes — so without a bound the file grows
/// forever. Lowest-scoring entries are dropped past this.
const MAX_RECENTS: usize = 200;

pub(crate) fn save_recent(command: &ExpandedCommand) -> anyhow::Result<()> {
    let mut recents = load_recents().unwrap_or_else(|_| vec![]);
    if let Some(recent_idx) = recents.iter().position(|r| r.brief == command.recent_key()) {
        let recent = recents.get_mut(recent_idx).unwrap();
        recent.frecency.register_access();
    } else {
        let mut frecency = Frecency::new();
        frecency.register_access();
        recents.push(Recent {
            brief: command.recent_key().to_string(),
            frecency,
        });
    }

    if recents.len() > MAX_RECENTS {
        // Re-sort first: the entry just touched sits wherever it was (or at
        // the very end when new), and load_recents' ordering predates this
        // access — truncating without re-sorting would drop the freshest
        // entry instead of the weakest.
        recents.sort_by(|a, b| {
            b.frecency
                .score()
                .partial_cmp(&a.frecency.score())
                .unwrap_or(Ordering::Equal)
        });
        recents.truncate(MAX_RECENTS);
    }

    let json = serde_json::to_string(&recents)?;
    let file_name = recent_file_name();
    std::fs::write(&file_name, json)?;
    Ok(())
}

/// Frecency score per `ExpandedCommand::recent_key`, from `recent-commands.json`. Empty when
/// nothing has been recorded yet (or the file is unreadable).
pub(crate) fn frecency_scores() -> HashMap<String, f64> {
    let mut scores = HashMap::new();
    if let Ok(recents) = load_recents() {
        for r in recents {
            let score = r.frecency.score();
            scores.insert(r.brief, score);
        }
    }
    scores
}

#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct UserPaletteEntry {
    pub brief: String,
    pub doc: Option<String>,
    pub action: KeyAssignment,
    pub icon: Option<String>,
}
impl_lua_conversion_dynamic!(UserPaletteEntry);

/// Whether `action` only means something inside the copy overlay, on its own
/// or as part of a chain such as "copy, then close". A chain that opens the
/// overlay itself works from anywhere.
fn is_copy_mode_action(action: &KeyAssignment) -> bool {
    fn opens_overlay(action: &KeyAssignment) -> bool {
        match action {
            KeyAssignment::ActivateCopyMode | KeyAssignment::Search(_) => true,
            KeyAssignment::Multiple(actions) => actions.iter().any(opens_overlay),
            _ => false,
        }
    }
    match action {
        KeyAssignment::CopyMode(_) => true,
        KeyAssignment::Multiple(actions) => {
            actions.iter().any(is_copy_mode_action) && !opens_overlay(action)
        }
        _ => false,
    }
}

pub(crate) fn build_commands(
    gui_window: GuiWin,
    pane: Option<MuxPane>,
    filter_copy_mode: bool,
) -> Vec<ExpandedCommand> {
    let mut commands = CommandDef::actions_for_palette_and_menubar(&config::configuration());
    // In the palette only groups carry an icon, and an entry from
    // `augment-command-palette` that names its own.
    for command in &mut commands {
        command.icon = None;
    }

    match config::run_immediate_with_lua_config(|lua| {
        let mut entries: Vec<UserPaletteEntry> = vec![];

        if let Some(lua) = lua {
            let result = config::lua::emit_sync_callback(
                &*lua,
                ("augment-command-palette".to_string(), (gui_window, pane)),
            )?;

            if !matches!(&result, mlua::Value::Nil) {
                entries = from_lua_value_dynamic(result)?;
            }
        }

        Ok(entries)
    }) {
        Ok(entries) => {
            for entry in entries {
                commands.push(ExpandedCommand {
                    brief: entry.brief.into(),
                    doc: match entry.doc {
                        Some(doc) => doc.into(),
                        None => "".into(),
                    },
                    action: entry.action,
                    keys: vec![],
                    menubar: &[],
                    icon: entry.icon.map(Cow::Owned),
                    accessory: None,
                    english_brief: None,
                });
            }
        }
        Err(err) => {
            log::warn!("augment-command-palette: {err:#}");
        }
    }

    commands.retain(|cmd| {
        !filter_copy_mode || !is_copy_mode_action(&cmd.action)
    });

    let scores = frecency_scores();

    commands.sort_by(|a, b| {
        match (scores.get(a.recent_key()), scores.get(b.recent_key())) {
            // Want descending frecency score, so swap a<->b
            // for the compare here
            (Some(a), Some(b)) => match b.partial_cmp(a) {
                Some(Ordering::Equal) | None => {}
                Some(ordering) => return ordering,
            },
            (Some(_), None) => return Ordering::Less,
            (None, Some(_)) => return Ordering::Greater,
            (None, None) => {}
        }

        // What plugins do comes after ThinkTerm's own, until it is used.
        let of_plugin =
            |command: &ExpandedCommand| matches!(command.action, KeyAssignment::PluginAction(_));
        match of_plugin(a).cmp(&of_plugin(b)) {
            Ordering::Equal => {}
            ordering => return ordering,
        }

        match a.menubar.cmp(&b.menubar) {
            Ordering::Equal => a.brief.cmp(&b.brief),
            ordering => ordering,
        }
    });

    commands
}

/// Give `commands` the chords they are bound to now. What `CommandDef` carries
/// is the defaults, which Settings → Keymap and the configuration file change.
pub(crate) fn apply_bound_chords(commands: &mut [ExpandedCommand], input_map: &InputMap) {
    for command in commands {
        command.keys = input_map
            .keys
            .default
            .iter()
            .filter(|(_, entry)| entry.action == command.action)
            .map(|((key, mods), _)| (*mods, key.clone()))
            .collect();
    }
}

/// The chords shown for a command: sorted to prefer the platform-native
/// modifier (⌘ on macOS), deduped by how they read, truncated to
/// `palette_max_key_assigments_for_action`.
pub(crate) fn shown_chords(
    command: &ExpandedCommand,
    config: &ConfigHandle,
) -> Vec<(Modifiers, window::KeyCode)> {
    let mut keys = command.keys.clone();

    keys.sort_by(|(a_mods, a_key), (b_mods, b_key)| {
        fn score_mods(mods: &Modifiers) -> usize {
            let mut score: usize = mods.bits() as usize;
            // Prefer keys with CMD on macOS, but not on other systems,
            // where CMD tends to be reserved by the desktop environment
            if cfg!(target_os = "macos") && mods.contains(Modifiers::SUPER) {
                score += 1000;
            } else if !cfg!(target_os = "macos") && !mods.contains(Modifiers::SUPER) {
                score += 1000;
            }
            score
        }

        let a_mods = score_mods(a_mods);
        let b_mods = score_mods(b_mods);

        match b_mods.cmp(&a_mods) {
            Ordering::Equal => {}
            ordering => return ordering,
        }

        a_key.cmp(&b_key)
    });

    let rendering = config.ui_key_cap_rendering;
    keys.dedup_by_key(|(mods, key)| crate::inputmap::tight_chord_label(key, *mods, rendering));
    keys.truncate(config.palette_max_key_assigments_for_action);
    keys
}

/// The human-readable shortcut column for a command, rendered with the
/// configured key-cap style (`⌘⇧P` under AppleSymbols) and joined with ", ".
/// `None` when the command has no bindings.
pub(crate) fn format_key_label(command: &ExpandedCommand, config: &ConfigHandle) -> Option<String> {
    let keys = shown_chords(command, config)
        .into_iter()
        .map(|(mods, keycode)| {
            crate::inputmap::tight_chord_label(&keycode, mods, config.ui_key_cap_rendering)
        })
        .collect::<Vec<_>>();

    if keys.is_empty() {
        None
    } else {
        Some(keys.join(", "))
    }
}

#[cfg(test)]
mod palette_tests {
    use super::{apply_bound_chords, is_copy_mode_action};
    use crate::commands::ExpandedCommand;
    use crate::inputmap::InputMap;
    use crate::native_settings::KeymapEntry;
    use config::keyassignment::{CopyModeAssignment, KeyAssignment, Pattern};
    use window::{KeyCode, Modifiers};

    #[test]
    fn a_command_shows_the_chords_it_is_bound_to_now() {
        let action = KeyAssignment::SpawnWindow;
        let mut commands = vec![ExpandedCommand {
            brief: "New Window".into(),
            doc: "".into(),
            keys: vec![(Modifiers::SUPER, KeyCode::Char('n'))],
            action: action.clone(),
            menubar: &[],
            icon: None,
            accessory: None,
            english_brief: None,
        }];
        let config = config::ConfigHandle::default_config();
        let map = InputMap::with_keymap(
            &config,
            &[
                KeymapEntry {
                    key: KeyCode::Char('y'),
                    mods: Modifiers::SUPER,
                    action,
                },
                KeymapEntry {
                    key: KeyCode::Char('n'),
                    mods: Modifiers::SUPER,
                    action: KeyAssignment::Nop,
                },
            ],
        );

        apply_bound_chords(&mut commands, &map);

        let keys = &commands[0].keys;
        assert!(keys.contains(&(Modifiers::SUPER, KeyCode::Char('y'))));
        assert!(!keys.contains(&(Modifiers::SUPER, KeyCode::Char('n'))));
    }

    #[test]
    fn a_chain_that_opens_the_overlay_is_not_copy_mode_only() {
        let move_up = KeyAssignment::CopyMode(CopyModeAssignment::MoveToScrollbackTop);
        assert!(!is_copy_mode_action(&KeyAssignment::Multiple(vec![
            KeyAssignment::ActivateCopyMode,
            move_up.clone(),
        ])));
        assert!(!is_copy_mode_action(&KeyAssignment::Multiple(vec![
            KeyAssignment::Search(Pattern::CurrentSelectionOrEmptyString),
            KeyAssignment::CopyMode(CopyModeAssignment::ClearPattern),
        ])));
    }

    #[test]
    fn a_chain_ending_the_copy_overlay_counts_as_copy_mode() {
        let close = KeyAssignment::CopyMode(CopyModeAssignment::Close);
        let chain = KeyAssignment::Multiple(vec![KeyAssignment::ScrollToBottom, close.clone()]);
        let nested = KeyAssignment::Multiple(vec![KeyAssignment::SpawnWindow, chain.clone()]);

        assert!(is_copy_mode_action(&close));
        assert!(is_copy_mode_action(&chain));
        assert!(is_copy_mode_action(&nested));
        assert!(!is_copy_mode_action(&KeyAssignment::Multiple(vec![
            KeyAssignment::ScrollToBottom,
        ])));
    }
}
