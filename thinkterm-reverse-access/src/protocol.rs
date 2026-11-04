use crate::channel::Deadline;
use crate::{Error, GrantRef, Permission, Result, Target, MAX_INPUT_BYTES, MAX_KEYS};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRef {
    pub grant: GrantRef,
    pub target: Target,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadOptions {
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub trim: bool,
    pub unwrap: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub name: String,
    pub modifiers: u16,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Command {
    List {
        after: Option<ExportRef>,
    },
    Read {
        export: ExportRef,
        options: ReadOptions,
    },
    Text {
        export: ExportRef,
        bytes: usize,
        no_paste: bool,
        enter: bool,
    },
    Keys {
        export: ExportRef,
        keys: Vec<Key>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimedCommand {
    command: Command,
    remaining_ms: u64,
}

impl TimedCommand {
    pub fn new(command: Command, deadline: &Deadline) -> Result<Self> {
        command.validate()?;
        let remaining_ms = deadline
            .remaining()?
            .as_millis()
            .min(u128::from(crate::REQUEST_TIMEOUT_SECS * 1000)) as u64;
        if remaining_ms == 0 {
            return Err(Error::Timeout);
        }
        Ok(Self {
            command,
            remaining_ms,
        })
    }

    pub fn receive(self) -> Result<(Command, Deadline)> {
        if self.remaining_ms == 0 || self.remaining_ms > crate::REQUEST_TIMEOUT_SECS * 1000 {
            return Err(Error::Protocol);
        }
        self.command.validate()?;
        Ok((
            self.command,
            Deadline::after(Duration::from_millis(self.remaining_ms))?,
        ))
    }
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::List { .. } => "List",
            Self::Read { .. } => "Read",
            Self::Text { .. } => "Text(..)",
            Self::Keys { .. } => "Keys(..)",
        })
    }
}

impl Command {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Text { bytes, .. } if *bytes > MAX_INPUT_BYTES => Err(Error::TooLarge),
            Self::Keys { keys, .. }
                if keys.is_empty()
                    || keys.len() > MAX_KEYS
                    || keys.iter().any(|key| {
                        key.name.is_empty()
                            || key.name.len() > 32
                            || key.name.chars().any(char::is_control)
                    }) =>
            {
                Err(Error::TooLarge)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportSummary {
    pub export: ExportRef,
    pub name: String,
    pub title: String,
    pub permission: Permission,
    pub remaining_ms: u64,
}

impl std::fmt::Debug for ExportSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ExportSummary(..)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputStatus {
    RejectedNotSubmitted,
    Submitted,
    PartiallySubmitted,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputResult {
    pub status: InputStatus,
    pub bytes: usize,
    pub enter_submitted: bool,
    pub paste_may_be_open: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Failure {
    Unavailable,
    Busy,
    TooLarge,
    Timeout,
    Protocol,
    Replay,
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        match error {
            Error::Busy => Self::Busy,
            Error::TooLarge => Self::TooLarge,
            Error::Timeout => Self::Timeout,
            Error::Protocol => Self::Protocol,
            Error::Replay => Self::Replay,
            _ => Self::Unavailable,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Reply {
    List {
        exports: Vec<ExportSummary>,
        more: bool,
    },
    Text {
        bytes: usize,
    },
    Input(InputResult),
    Failed(Failure),
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::List { .. } => "List(..)",
            Self::Text { .. } => "Text(..)",
            Self::Input(_) => "Input(..)",
            Self::Failed(_) => "Failed(..)",
        })
    }
}

/// How long `--enter` waits after `len` bytes of text: a moment, and a
/// little more for a long text a program takes longer to read, up to two
/// seconds. Sent together, a program reading a paste can take the Enter as
/// part of it.
pub fn enter_delay(len: usize) -> Duration {
    Duration::from_millis((300 + len as u64 / 1024 * 50).min(2000))
}

pub fn display_text(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c,
        '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(max_chars)
        .collect()
}
