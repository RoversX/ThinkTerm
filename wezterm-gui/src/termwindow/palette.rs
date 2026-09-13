//! Data layer for the command palette: command enumeration, recents/frecency
//! tracking and shortcut formatting. The UI lives in
//! `termwindow/ui/command_palette.rs`.

use crate::commands::{CommandDef, ExpandedCommand};
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
    if let Some(recent_idx) = recents.iter().position(|r| r.brief == command.brief) {
        let recent = recents.get_mut(recent_idx).unwrap();
        recent.frecency.register_access();
    } else {
        let mut frecency = Frecency::new();
        frecency.register_access();
        recents.push(Recent {
            brief: command.brief.to_string(),
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

/// Frecency score per command brief, from `recent-commands.json`. Empty when
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

pub(crate) fn build_commands(
    gui_window: GuiWin,
    pane: Option<MuxPane>,
    filter_copy_mode: bool,
) -> Vec<ExpandedCommand> {
    let mut commands = CommandDef::actions_for_palette_and_menubar(&config::configuration());

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
                });
            }
        }
        Err(err) => {
            log::warn!("augment-command-palette: {err:#}");
        }
    }

    commands.retain(|cmd| {
        if filter_copy_mode {
            !matches!(cmd.action, KeyAssignment::CopyMode(_))
        } else {
            true
        }
    });

    let scores = frecency_scores();

    commands.sort_by(|a, b| {
        match (scores.get(&*a.brief), scores.get(&*b.brief)) {
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

        match a.menubar.cmp(&b.menubar) {
            Ordering::Equal => a.brief.cmp(&b.brief),
            ordering => ordering,
        }
    });

    commands
}

/// The human-readable shortcut column for a command: its chords sorted to
/// prefer the platform-native modifier (⌘ on macOS), rendered with the
/// configured key-cap style (`⌘⇧P` under AppleSymbols), deduped, truncated to
/// `palette_max_key_assigments_for_action` and joined with ", ".
/// `None` when the command has no bindings.
pub(crate) fn format_key_label(command: &ExpandedCommand, config: &ConfigHandle) -> Option<String> {
    if command.keys.is_empty() {
        return None;
    }
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

    let separator = if config.ui_key_cap_rendering == ::window::UIKeyCapRendering::AppleSymbols {
        " "
    } else {
        "-"
    };

    let mut keys = keys
        .into_iter()
        .map(|(mods, keycode)| {
            let mut mod_string = mods.to_string_with_separator(::window::ModifierToStringArgs {
                separator,
                want_none: false,
                ui_key_cap_rendering: Some(config.ui_key_cap_rendering),
            });
            if !mod_string.is_empty() {
                mod_string.push_str(separator);
            }
            let keycode = crate::inputmap::ui_key(&keycode, config.ui_key_cap_rendering);
            format!("{mod_string}{keycode}")
        })
        .collect::<Vec<_>>();

    keys.dedup();
    keys.truncate(config.palette_max_key_assigments_for_action);

    if keys.is_empty() {
        None
    } else {
        Some(keys.join(", "))
    }
}
