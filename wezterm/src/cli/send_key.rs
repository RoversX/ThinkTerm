use crate::cli::wait::{exit_with, parse_timeout, shell_commands, wait_for_command, with_deadline};
use crate::cli::UsageError;
use anyhow::{anyhow, bail};
use clap::Parser;
use codec::{InputSerial, SendKeyDown};
use mux::pane::PaneId;
use std::time::{Duration, Instant};
use termwiz::input::{KeyCode, KeyEvent, Modifiers};
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct SendKey {
    /// Specify the target pane.
    /// The default is to use the current pane based on the
    /// environment variable WEZTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// The pane by name rather than id: its label, its thread's name (a
    /// thread with one pane), or `thread/label`.
    #[arg(long, conflicts_with = "pane_id")]
    pane: Option<String>,

    /// After the keys, wait for the command they start in the pane's shell
    /// to finish, and exit with its status (125 when the shell gave none):
    /// `send-key --wait enter` runs what `send-text` typed. Needs a shell
    /// that reports its commands (the shell integration's OSC 133 marks).
    #[arg(long)]
    wait: bool,

    /// With --wait, give up after this long, exiting with status 124: a
    /// number of seconds, or a number with s, m or h after it. `--for` is
    /// the same.
    #[arg(long, visible_alias = "for", value_parser = parse_timeout, requires = "wait")]
    timeout: Option<Duration>,

    /// The keys to press, one after another, each written as modifiers and
    /// a key joined by `+`: `enter`, `ctrl+c`, `up`, `shift+tab`, `alt+f`,
    /// `f5`. Modifiers are ctrl, alt (or opt), shift and cmd (or super).
    /// Keys are a character, or enter, tab, escape, backspace, delete,
    /// space, up, down, left, right, home, end, pageup, pagedown, insert
    /// and f1 to f24. Each is encoded the way the program in the pane asked
    /// for keys, as if typed.
    #[arg(required = true, num_args = 1..)]
    keys: Vec<String>,
}

impl SendKey {
    pub async fn run(self, client: Client) -> anyhow::Result<()> {
        let deadline = self.timeout.map(|t| Instant::now() + t);
        let events = self
            .keys
            .iter()
            .map(|key| parse_key(key))
            .collect::<anyhow::Result<Vec<_>>>()
            .map_err(|err| UsageError(format!("{err:#}")))?;

        if !self.wait {
            let (client, pane_id) = crate::cli::target::resolve_reaching_servers(
                client,
                self.pane_id,
                self.pane.as_deref(),
            )
            .await?;
            return send_keys(&client, pane_id, events).await;
        }
        let code = with_deadline(deadline, async {
            // Finding the focused pane asks the server too.
            let (client, pane_id) = crate::cli::target::resolve_reaching_servers(
                client.clone(),
                self.pane_id,
                self.pane.as_deref(),
            )
            .await?;
            // Counted before the keys go, so a command that finishes at
            // once is not missed.
            let before = shell_commands(&client, pane_id).await?.finished;
            send_keys(&client, pane_id, events).await?;
            wait_for_command(&client, pane_id, before, deadline).await
        })
        .await?;
        exit_with(code)
    }
}

async fn send_keys(client: &Client, pane_id: PaneId, events: Vec<KeyEvent>) -> anyhow::Result<()> {
    for event in events {
        client
            .key_down(SendKeyDown {
                pane_id,
                event,
                input_serial: InputSerial::now(),
            })
            .await?;
    }
    Ok(())
}

/// One key as the command line writes it: modifiers and a key joined by
/// `+`, any case. `ctrl++` is ctrl and plus.
fn parse_key(text: &str) -> anyhow::Result<KeyEvent> {
    let (mods, key) = if text == "+" {
        ("", "+")
    } else if let Some(mods) = text.strip_suffix("++") {
        (mods, "+")
    } else {
        match text.rsplit_once('+') {
            Some((mods, key)) => (mods, key),
            None => ("", text),
        }
    };

    let mut modifiers = Modifiers::NONE;
    for name in mods.split('+').filter(|name| !name.is_empty()) {
        modifiers |= match name.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Modifiers::CTRL,
            "alt" | "opt" | "option" | "meta" => Modifiers::ALT,
            "shift" => Modifiers::SHIFT,
            "cmd" | "command" | "super" | "win" => Modifiers::SUPER,
            _ => bail!("{text:?}: {name:?} is not a modifier (ctrl, alt, shift or cmd)"),
        };
    }

    let mut chars = key.chars();
    let key = match (chars.next(), chars.next()) {
        (Some(c), None) => KeyCode::Char(c),
        _ => match key.to_ascii_lowercase().as_str() {
            "enter" | "return" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "escape" | "esc" => KeyCode::Escape,
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "space" => KeyCode::Char(' '),
            "up" => KeyCode::UpArrow,
            "down" => KeyCode::DownArrow,
            "left" => KeyCode::LeftArrow,
            "right" => KeyCode::RightArrow,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "insert" => KeyCode::Insert,
            name => match name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                Some(n @ 1..=24) => KeyCode::Function(n),
                _ => return Err(anyhow!("{text:?}: {key:?} is not a key")),
            },
        },
    };
    Ok(KeyEvent { key, modifiers })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: KeyCode, modifiers: Modifiers) -> KeyEvent {
        KeyEvent { key, modifiers }
    }

    #[test]
    fn keys_are_read_the_way_shortcuts_are_written() {
        assert_eq!(
            parse_key("enter").unwrap(),
            key(KeyCode::Enter, Modifiers::NONE)
        );
        assert_eq!(
            parse_key("Return").unwrap(),
            key(KeyCode::Enter, Modifiers::NONE)
        );
        assert_eq!(
            parse_key("ctrl+c").unwrap(),
            key(KeyCode::Char('c'), Modifiers::CTRL)
        );
        assert_eq!(
            parse_key("Shift+Tab").unwrap(),
            key(KeyCode::Tab, Modifiers::SHIFT)
        );
        assert_eq!(
            parse_key("ctrl+alt+delete").unwrap(),
            key(KeyCode::Delete, Modifiers::CTRL | Modifiers::ALT)
        );
        assert_eq!(
            parse_key("up").unwrap(),
            key(KeyCode::UpArrow, Modifiers::NONE)
        );
        assert_eq!(
            parse_key("f12").unwrap(),
            key(KeyCode::Function(12), Modifiers::NONE)
        );
        assert_eq!(
            parse_key("space").unwrap(),
            key(KeyCode::Char(' '), Modifiers::NONE)
        );
        assert_eq!(
            parse_key("q").unwrap(),
            key(KeyCode::Char('q'), Modifiers::NONE)
        );
        assert_eq!(
            parse_key("/").unwrap(),
            key(KeyCode::Char('/'), Modifiers::NONE)
        );
        assert_eq!(
            parse_key("+").unwrap(),
            key(KeyCode::Char('+'), Modifiers::NONE)
        );
        assert_eq!(
            parse_key("ctrl++").unwrap(),
            key(KeyCode::Char('+'), Modifiers::CTRL)
        );
        assert_eq!(
            parse_key("cmd+k").unwrap(),
            key(KeyCode::Char('k'), Modifiers::SUPER)
        );
    }

    #[test]
    fn unknown_keys_and_modifiers_are_refused() {
        assert!(parse_key("ctrl+").is_err());
        assert!(parse_key("hyper+c").is_err());
        assert!(parse_key("f25").is_err());
        assert!(parse_key("enterr").is_err());
    }
}
