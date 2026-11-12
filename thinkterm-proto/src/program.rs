//! Foreground program wire types.
//!
//! The mux that owns a pane reports which program leads its terminal; a
//! client decides for itself what that means for presentation (the icon on
//! the pane's tab, say). Only the facts travel.
//!
//! Varbincode is positional: **field order is the wire contract**. Bump the
//! codec version when changing anything here.

use serde::{Deserialize, Serialize};

/// Longest name either field may carry: a file name, which no platform
/// lets exceed 255 bytes.
pub const MAX_PROGRAM_NAME_BYTES: usize = 255;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ForegroundProgram {
    /// File name of the executable leading the terminal's foreground
    /// process group: `vim`, `node`, `python3.12`, `zsh`.
    pub executable: String,
    /// What that executable runs, when it is an interpreter, a shell or a
    /// launcher: the script's file name (`npm` for `node …/bin/npm`,
    /// `deploy.sh`), the module (`pytest` for `python -m pytest`) or the
    /// command (`vim` for `sudo vim`). `None` when it runs nothing of the
    /// kind, as an interactive shell does.
    pub runs: Option<String>,
    /// What it is doing, read off its screen by task rules where the pane
    /// lives, when it is a tool that has some (a package manager, say).
    pub task: Option<TaskReading>,
}

/// Longest task-rules id or display name, in bytes.
pub const MAX_TASK_NAME_BYTES: usize = 64;
/// Longest `TaskReading::detail`, in characters: a few words.
pub const MAX_TASK_DETAIL_CHARS: usize = 80;

/// What a tool is doing, as far as its screen says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum TaskState {
    #[default]
    Running,
    /// Stopped to ask the user something.
    Waiting,
    /// Said it failed.
    Error,
}

/// One reading of a tool's screen.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct TaskReading {
    /// Stable task-rules id, independent of the tool's display name.
    pub id: String,
    /// What to call the tool: `apt`, `brew`.
    pub name: String,
    pub state: TaskState,
    /// 0 to 100.
    pub percent: Option<u8>,
    /// How many of how many.
    pub count: Option<(u32, u32)>,
    /// What it is doing now, in a few words.
    pub detail: Option<String>,
}

/// A character that should not be in a task reading's words: a control
/// character, or one that is not seen but changes what is -- the direction
/// text runs in, or what joins to what. The words come off a screen, which
/// any program can print to.
pub fn is_hidden_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

impl TaskReading {
    fn within_budget(&self) -> bool {
        !self.id.is_empty()
            && self.id.len() <= MAX_TASK_NAME_BYTES
            && !self.id.chars().any(is_hidden_char)
            && !self.name.is_empty()
            && !self.name.chars().any(is_hidden_char)
            && self.detail.as_deref().is_none_or(|detail| !detail.chars().any(is_hidden_char))
            && self.name.len() <= MAX_TASK_NAME_BYTES
            && self.percent.is_none_or(|percent| percent <= 100)
            && self.count.is_none_or(|(done, total)| total > 0 && done <= total)
            && self
                .detail
                .as_deref()
                .is_none_or(|detail| detail.chars().count() <= MAX_TASK_DETAIL_CHARS)
    }
}

impl ForegroundProgram {
    /// Both names are plain, non-empty file names within the budget, and
    /// a task reading, if any, within its own.
    pub fn within_budget(&self) -> bool {
        is_program_name(&self.executable)
            && self.runs.as_deref().is_none_or(is_program_name)
            && self.task.as_ref().is_none_or(TaskReading::within_budget)
    }
}

fn is_program_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_PROGRAM_NAME_BYTES && !name.contains(['/', '\\', '\0'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_ids_must_be_bounded_and_visible() {
        let mut reading = TaskReading {
            id: "brew".into(),
            name: "Homebrew".into(),
            ..Default::default()
        };
        assert!(reading.within_budget());
        for id in [
            String::new(),
            "x".repeat(MAX_TASK_NAME_BYTES + 1),
            "brew\n".into(),
            "brew\u{202E}".into(),
        ] {
            reading.id = id;
            assert!(!reading.within_budget());
        }
    }

    #[test]
    fn budget_rejects_paths_and_oversized_names() {
        let program = |executable: &str, runs: Option<&str>| ForegroundProgram {
            executable: executable.to_string(),
            runs: runs.map(str::to_string),
            task: None,
        };
        assert!(program("node", Some("npm")).within_budget());
        let reading = |detail: &str| ForegroundProgram {
            task: Some(TaskReading {
                id: "apt".to_string(),
                name: "apt".to_string(),
                detail: Some(detail.to_string()),
                ..TaskReading::default()
            }),
            ..program("apt", None)
        };
        assert!(reading("Setting up libfoo").within_budget());
        // Text that reads one way and runs another is turned away.
        assert!(!reading("Setting up \u{202E}oof").within_budget());
        assert!(!reading("line\nbreak").within_budget());
        assert!(program("zsh", None).within_budget());
        assert!(!program("", None).within_budget());
        assert!(!program("node", Some("")).within_budget());
        assert!(!program("/usr/bin/node", None).within_budget());
        assert!(!program("node", Some("bin/npm")).within_budget());
        assert!(!program(&"x".repeat(MAX_PROGRAM_NAME_BYTES + 1), None).within_budget());
    }
}
