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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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
}

impl ForegroundProgram {
    /// Both names are plain, non-empty file names within the budget.
    pub fn within_budget(&self) -> bool {
        is_program_name(&self.executable) && self.runs.as_deref().is_none_or(is_program_name)
    }
}

fn is_program_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_PROGRAM_NAME_BYTES && !name.contains(['/', '\\', '\0'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_rejects_paths_and_oversized_names() {
        let program = |executable: &str, runs: Option<&str>| ForegroundProgram {
            executable: executable.to_string(),
            runs: runs.map(str::to_string),
        };
        assert!(program("node", Some("npm")).within_budget());
        assert!(program("zsh", None).within_budget());
        assert!(!program("", None).within_budget());
        assert!(!program("node", Some("")).within_budget());
        assert!(!program("/usr/bin/node", None).within_budget());
        assert!(!program("node", Some("bin/npm")).within_budget());
        assert!(!program(&"x".repeat(MAX_PROGRAM_NAME_BYTES + 1), None).within_budget());
    }
}
