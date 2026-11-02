use anyhow::Context;
use clap::Parser;
use codec::{InputSerial, SendKeyDown};
use mux::pane::PaneId;
use std::io::Read;
use std::time::Duration;
use termwiz::input::{KeyCode, KeyEvent, Modifiers};
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct SendText {
    /// Specify the target pane.
    /// The default is to use the current pane based on the
    /// environment variable WEZTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// The pane by name rather than id: its label, its thread's name (a
    /// thread with one pane), or `thread/label`.
    #[arg(long, conflicts_with = "pane_id")]
    pane: Option<String>,

    /// Send the text directly, rather than as a bracketed paste.
    #[arg(long)]
    no_paste: bool,

    /// Then press Enter, as `send-key enter` does, a moment after the text:
    /// a program that reads it as a paste would otherwise take the Enter as
    /// part of it.
    #[arg(long)]
    enter: bool,

    /// The text to send. If omitted, will read the text from stdin.
    text: Option<String>,
}

impl SendText {
    pub async fn run(self, client: Client) -> anyhow::Result<()> {
        let (client, pane_id) = crate::cli::target::resolve_reaching_servers(
            client,
            self.pane_id,
            self.pane.as_deref(),
        )
        .await?;

        let data = match self.text {
            Some(text) => text,
            None => {
                let mut text = String::new();
                std::io::stdin()
                    .read_to_string(&mut text)
                    .context("reading stdin")?;
                text
            }
        };

        if self.no_paste {
            client
                .write_to_pane(codec::WriteToPane {
                    pane_id,
                    data: data.as_bytes().to_vec(),
                })
                .await?;
        } else {
            client
                .send_paste(codec::SendPaste {
                    pane_id,
                    data: data.clone(),
                })
                .await?;
        }
        if self.enter {
            smol::Timer::after(enter_delay(data.len())).await;
            client
                .key_down(SendKeyDown {
                    pane_id,
                    event: KeyEvent {
                        key: KeyCode::Enter,
                        modifiers: Modifiers::NONE,
                    },
                    input_serial: InputSerial::now(),
                })
                .await?;
        }
        Ok(())
    }
}

/// How long `--enter` waits after `len` bytes of text: a moment, and a
/// little more for a long text a program takes longer to read, up to two
/// seconds.
fn enter_delay(len: usize) -> Duration {
    Duration::from_millis((300 + len as u64 / 1024 * 50).min(2000))
}

#[cfg(test)]
mod tests {
    use super::enter_delay;
    use std::time::Duration;

    #[test]
    fn the_enter_waits_longer_for_longer_text_within_bounds() {
        assert_eq!(enter_delay(0), Duration::from_millis(300));
        assert_eq!(enter_delay(10 * 1024), Duration::from_millis(800));
        assert_eq!(enter_delay(10 << 20), Duration::from_secs(2));
    }
}
