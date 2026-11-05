use super::*;
use crate::terminalstate::performer::Performer;
use std::sync::Arc;
use wezterm_escape_parser::parser::{ParsedAction, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum ClipboardSelection {
    Clipboard,
    PrimarySelection,
}

pub trait Clipboard: Send + Sync {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        data: Option<String>,
    ) -> anyhow::Result<()>;
}

impl Clipboard for Box<dyn Clipboard> {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        data: Option<String>,
    ) -> anyhow::Result<()> {
        self.as_ref().set_contents(selection, data)
    }
}

pub trait DeviceControlHandler: Send + Sync {
    fn handle_device_control(&mut self, _control: wezterm_escape_parser::DeviceControlMode);
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum Progress {
    #[default]
    None,
    Percentage(u8),
    Error(u8),
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub enum Alert {
    Bell,
    ToastNotification {
        /// The title text for the notification.
        title: Option<String>,
        /// The message body
        body: String,
        /// Whether clicking on the notification should focus the
        /// window/tab/pane that generated it
        focus: bool,
    },
    CurrentWorkingDirectoryChanged,
    IconTitleChanged(Option<String>),
    WindowTitleChanged(String),
    TabTitleChanged(Option<String>),
    /// When the color palette has been updated
    PaletteChanged,
    /// A UserVar has changed value
    SetUserVar {
        name: String,
        value: String,
    },
    /// When something bumps the seqno in the terminal model and
    /// the terminal is not focused
    OutputSinceFocusLost,
    /// A change to the progress bar state
    Progress(Progress),
    /// The shell reported (OSC 133) that a command finished, with the
    /// status it gave, or `None` when it gave none: its end mark left the
    /// status out, or a new prompt ended the command.
    CommandFinished {
        exit_code: Option<i32>,
    },
}

/// How far the shell in a terminal has got, as far as it says with the
/// OSC 133 semantic prompt marks. All zero until the first mark arrives.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ShellCommands {
    /// The shell has sent a mark, so the rest of this means something.
    pub reported: bool,
    /// Commands finished since the first mark. The shell integration also
    /// reports one for a prompt answered with an empty line.
    pub finished: u64,
    /// A command has started (`133;C`) and not yet finished.
    pub running: bool,
    /// The status the last finished command reported.
    pub last_exit: Option<i32>,
}

pub trait AlertHandler: Send + Sync {
    fn alert(&mut self, alert: Alert);
}

pub trait DownloadHandler: Send + Sync {
    fn save_to_downloads(&self, name: Option<String>, data: Vec<u8>);
}

/// Represents an instance of a terminal emulator.
pub struct Terminal {
    /// The terminal model/state
    state: TerminalState,
    /// Baseline terminal escape sequence parser
    parser: Parser,
}

impl Deref for Terminal {
    type Target = TerminalState;

    fn deref(&self) -> &TerminalState {
        &self.state
    }
}

impl DerefMut for Terminal {
    fn deref_mut(&mut self) -> &mut TerminalState {
        &mut self.state
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, FromDynamic, ToDynamic)]
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub struct TerminalSize {
    pub rows: usize,
    pub cols: usize,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub dpi: u32,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        }
    }
}

impl Terminal {
    /// Construct a new Terminal.
    /// `physical_rows` and `physical_cols` describe the dimensions
    /// of the visible portion of the terminal display in terms of
    /// the number of text cells.
    ///
    /// `pixel_width` and `pixel_height` describe the dimensions of
    /// that same visible area but in pixels.
    ///
    /// `term_program` and `term_version` are required to identify
    /// the host terminal program; they are used to respond to the
    /// terminal identification sequence `\033[>q`.
    ///
    /// `writer` is anything that implements `std::io::Write`; it
    /// is used to send input to the connected program; both keyboard
    /// and mouse input is encoded and written to that stream, as
    /// are answerback responses to a number of escape sequences.
    pub fn new(
        size: TerminalSize,
        config: Arc<dyn TerminalConfiguration + Send + Sync>,
        term_program: &str,
        term_version: &str,
        // writing to the writer sends data to input of the pty
        writer: Box<dyn std::io::Write + Send>,
    ) -> Terminal {
        let mut parser = Parser::new();
        parser.set_text_keeps_line_controls(true);
        Terminal {
            state: TerminalState::new(size, config, term_program, term_version, writer),
            parser,
        }
    }

    /// Feed the terminal parser a slice of bytes from the output
    /// of the associated program.
    /// The slice is not required to be a complete sequence of escape
    /// characters; it is valid to feed in chunks of data as they arrive.
    /// The output is parsed and applied to the terminal model.
    pub fn advance_bytes<B: AsRef<[u8]>>(&mut self, bytes: B) {
        self.state.increment_seqno();
        {
            let bytes = bytes.as_ref();

            let mut performer = Performer::new(&mut self.state);

            self.parser
                .parse_with_borrowed_text(bytes, |event| match event {
                    ParsedAction::Action(action) => performer.perform(action),
                    ParsedAction::Print(text) => performer.print_run(text),
                });
        }
        self.state.kitty_scene_refresh();
        self.trigger_unseen_output_notif();
    }

    pub fn perform_actions(&mut self, mut actions: Vec<wezterm_escape_parser::Action>) {
        self.perform_actions_in_place(&mut actions);
    }

    /// `perform_actions`, leaving `actions` empty with its allocation kept so
    /// a caller that applies batch after batch does not regrow it each time.
    pub fn perform_actions_in_place(&mut self, actions: &mut Vec<wezterm_escape_parser::Action>) {
        self.state.increment_seqno();
        {
            let mut performer = Performer::new(&mut self.state);
            for action in actions.drain(..) {
                performer.perform(action);
            }
        }
        self.state.kitty_scene_refresh();
        self.trigger_unseen_output_notif();
    }
}
