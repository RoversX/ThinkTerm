use crate::scripting::guiwin::GuiWin;
use config::keyassignment::{KeyAssignment, PromptInputLine};
use mux::termwiztermtab::TermWizTerminal;
use mux_lua::MuxPane;
use std::rc::Rc;
use termwiz::cell::{unicode_column_width, AttributeChange, CellAttributes};
use termwiz::color::ColorAttribute;
use termwiz::input::{InputEvent, KeyCode, KeyEvent, Modifiers, MouseButtons, MouseEvent};
use termwiz::lineedit::*;
use termwiz::surface::{Change, Position};
use termwiz::terminal::Terminal;

struct PromptHost {
    history: BasicHistory,
}

impl PromptHost {
    fn new() -> Self {
        Self {
            history: BasicHistory::default(),
        }
    }
}

impl LineEditorHost for PromptHost {
    fn history(&mut self) -> &mut dyn History {
        &mut self.history
    }

    fn resolve_action(
        &mut self,
        event: &InputEvent,
        editor: &mut LineEditor<'_>,
    ) -> Option<Action> {
        let (line, _cursor) = editor.get_line_and_cursor();
        if line.is_empty()
            && matches!(
                event,
                InputEvent::Key(KeyEvent {
                    key: KeyCode::Escape,
                    ..
                })
            )
        {
            Some(Action::Cancel)
        } else {
            None
        }
    }
}

pub fn show_line_prompt_overlay(
    term: TermWizTerminal,
    args: PromptInputLine,
    window: GuiWin,
    pane: MuxPane,
) -> anyhow::Result<()> {
    let name = match *args.action {
        KeyAssignment::EmitEvent(id) => id,
        _ => anyhow::bail!(
            "PromptInputLine requires action to be defined by wezterm.action_callback"
        ),
    };

    let line = read_line_prompt_overlay(
        term,
        &args.description,
        &args.prompt,
        args.initial_value.as_deref(),
    )?;

    promise::spawn::spawn_into_main_thread(async move {
        trampoline(name, window, pane, line);
        anyhow::Result::<()>::Ok(())
    })
    .detach();

    Ok(())
}

pub fn read_line_prompt_overlay(
    mut term: TermWizTerminal,
    description: &str,
    prompt: &str,
    initial_value: Option<&str>,
) -> anyhow::Result<Option<String>> {
    term.no_grab_mouse_in_raw_mode();
    let mut text = description.replace("\r\n", "\n").replace("\n", "\r\n");
    text.push_str("\r\n");
    term.render(&[Change::Text(text)])?;

    let mut host = PromptHost::new();
    let mut editor = LineEditor::new(&mut term);
    editor.set_prompt(prompt);
    Ok(editor.read_line_with_optional_initial_value(&mut host, initial_value)?)
}

/// Does the typed text read as a filesystem path (rather than a filter term
/// for the candidate list)?
fn looks_like_path(input: &str) -> bool {
    let input = input.trim_start();
    input.starts_with('/') || input.starts_with('~') || input.starts_with('.')
}

/// An interactive line prompt with a candidate list: Up/Down (or Ctrl-P/N)
/// moves the selection, typing fuzzy-filters the candidates, and text that
/// looks like a path (`~/...`, `/...`, `./...`) is offered verbatim as the
/// first entry. Enter confirms the highlighted entry; Esc or Ctrl-C cancels.
/// Returns the chosen string, or None when cancelled.
pub fn pick_path_prompt_overlay(
    mut term: TermWizTerminal,
    description: &str,
    prompt: &str,
    candidates: Vec<String>,
) -> anyhow::Result<Option<String>> {
    term.set_raw_mode()?;

    let description = description.replace("\r\n", "\n");
    let description_rows = description.split('\n').count();
    let hint = "Up/Down select, Enter confirm, Esc cancel";

    fn picked_value(path_input: bool, input: &str, filtered: &[&String], idx: usize) -> String {
        if path_input && idx == 0 {
            input.trim().to_string()
        } else {
            filtered[idx - usize::from(path_input)].to_string()
        }
    }

    let mut input = String::new();
    let mut active = 0usize;
    let mut top_row = 0usize;

    loop {
        let filtered: Vec<&String> = if input.trim().is_empty() {
            candidates.iter().collect()
        } else {
            let pattern = super::selector::matcher_pattern(input.trim());
            let mut scored: Vec<(u32, &String)> = candidates
                .iter()
                .filter_map(|c| super::selector::matcher_score(&pattern, c).map(|s| (s, c)))
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            scored.into_iter().map(|(_, c)| c).collect()
        };
        let path_input = looks_like_path(&input);
        let n_rows = filtered.len() + usize::from(path_input);
        if active >= n_rows {
            active = n_rows.saturating_sub(1);
        }

        let size = term.get_screen_size()?;
        let max_width = size.cols.saturating_sub(2).max(8);
        let list_capacity = size
            .rows
            .saturating_sub(description_rows + 2)
            .max(1)
            .min(n_rows);
        if active < top_row {
            top_row = active;
        } else if active >= top_row + list_capacity {
            top_row = active + 1 - list_capacity;
        }

        let mut changes = vec![
            Change::ClearScreen(ColorAttribute::Default),
            Change::CursorPosition {
                x: Position::Absolute(0),
                y: Position::Absolute(0),
            },
            Change::Text(format!("{}\r\n", description.replace('\n', "\r\n"))),
            Change::Attribute(AttributeChange::Intensity(
                termwiz::cell::Intensity::Half,
            )),
            Change::Text(format!("{hint}\r\n")),
            Change::AllAttributes(CellAttributes::default()),
            Change::Text(format!("{prompt}{input}\r\n")),
        ];
        for (row, idx) in (top_row..(top_row + list_capacity).min(n_rows)).enumerate() {
            let _ = row;
            let label = if path_input && idx == 0 {
                format!("Use path: {}", input.trim())
            } else {
                filtered[idx - usize::from(path_input)].to_string()
            };
            let label: String = label.chars().take(max_width).collect();
            if idx == active {
                changes.push(Change::Attribute(AttributeChange::Reverse(true)));
                changes.push(Change::Text(format!(" {label} \r\n")));
                changes.push(Change::Attribute(AttributeChange::Reverse(false)));
            } else {
                changes.push(Change::Text(format!(" {label} \r\n")));
            }
        }
        changes.push(Change::CursorPosition {
            x: Position::Absolute(unicode_column_width(prompt, None) + unicode_column_width(&input, None)),
            y: Position::Absolute(description_rows + 1),
        });
        term.render(&changes)?;

        let Some(event) = term.poll_input(None)? else {
            return Ok(None);
        };
        match event {
            InputEvent::Key(KeyEvent { key, modifiers }) => match (key, modifiers) {
                (KeyCode::Escape, _) => return Ok(None),
                (KeyCode::Char('c'), Modifiers::CTRL)
                | (KeyCode::Char('g'), Modifiers::CTRL) => return Ok(None),
                (KeyCode::UpArrow, _) | (KeyCode::Char('p'), Modifiers::CTRL) => {
                    active = active.saturating_sub(1);
                }
                (KeyCode::DownArrow, _) | (KeyCode::Char('n'), Modifiers::CTRL) => {
                    if n_rows > 0 {
                        active = (active + 1).min(n_rows - 1);
                    }
                }
                (KeyCode::Enter, _) => {
                    if n_rows == 0 {
                        let text = input.trim();
                        if !text.is_empty() {
                            return Ok(Some(text.to_string()));
                        }
                    } else {
                        return Ok(Some(picked_value(path_input, &input, &filtered, active)));
                    }
                }
                (KeyCode::Backspace, _) => {
                    input.pop();
                    active = 0;
                    top_row = 0;
                }
                (KeyCode::Char('u'), Modifiers::CTRL) => {
                    input.clear();
                    active = 0;
                    top_row = 0;
                }
                (KeyCode::Char(c), mods)
                    if !mods.intersects(
                        Modifiers::CTRL | Modifiers::ALT | Modifiers::SUPER,
                    ) =>
                {
                    input.push(c);
                    active = 0;
                    top_row = 0;
                }
                _ => {}
            },
            InputEvent::Mouse(MouseEvent {
                mouse_buttons, ..
            }) if mouse_buttons.contains(MouseButtons::VERT_WHEEL) => {
                if mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE) {
                    top_row = top_row.saturating_sub(1);
                } else {
                    top_row = (top_row + 1).min(n_rows.saturating_sub(list_capacity));
                }
                active = active.clamp(top_row, top_row + list_capacity.saturating_sub(1));
            }
            InputEvent::Mouse(MouseEvent {
                y, mouse_buttons, ..
            }) => {
                // Rows above the list: description, hint, then the input line.
                let list_top = description_rows + 2;
                if let Some(row) = (y as usize).checked_sub(list_top) {
                    let idx = top_row + row;
                    if idx < n_rows {
                        active = idx;
                        if mouse_buttons == MouseButtons::LEFT {
                            return Ok(Some(picked_value(path_input, &input, &filtered, idx)));
                        }
                    }
                }
            }
            InputEvent::Resized { .. } => {}
            _ => {}
        }
    }
}

fn trampoline(name: String, window: GuiWin, pane: MuxPane, line: Option<String>) {
    promise::spawn::spawn(async move {
        config::with_lua_config_on_main_thread(move |lua| do_event(lua, name, window, pane, line))
            .await
    })
    .detach();
}

async fn do_event(
    lua: Option<Rc<mlua::Lua>>,
    name: String,
    window: GuiWin,
    pane: MuxPane,
    line: Option<String>,
) -> anyhow::Result<()> {
    if let Some(lua) = lua {
        let args = lua.pack_multi((window, pane, line))?;

        if let Err(err) = config::lua::emit_event(&lua, (name.clone(), args)).await {
            log::error!("while processing {} event: {:#}", name, err);
        }
    }

    Ok(())
}
