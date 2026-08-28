pub mod agent_panel;
pub mod context_menu;
pub mod folder_problem;
pub mod icons;
pub mod platform_chrome;
pub mod right_sidebar;
pub mod sidebar;
pub mod status_icon;
pub mod tokens;

use crate::utilsprites::RenderMetrics;

pub use right_sidebar::{
    right_sidebar_file_preview_width, right_sidebar_note_pane_width_for_dpi,
    right_sidebar_width_for_metrics,
};
pub use sidebar::workspace_sidebar_width_for_metrics;

pub fn pane_nav_bar_height_for_metrics(render_metrics: RenderMetrics) -> usize {
    // Match the primary tab content height so both tab levels keep the same
    // vertical rhythm on every monitor DPI.
    let cell_height = render_metrics.cell_size.height.max(1) as usize;
    cell_height * 2
}

impl crate::TermWindow {
    pub(crate) fn pane_nav_bar_height(&self) -> usize {
        let metrics = self
            .fonts
            .title_font_with_size(crate::native_settings::pane_header_font_size())
            .map(|font| RenderMetrics::with_font_metrics(&font.metrics()))
            .unwrap_or(self.render_metrics);
        pane_nav_bar_height_for_metrics(metrics)
    }
}

pub fn terminal_title_for_display(title: &str) -> &str {
    let title = title.trim();
    if title.is_empty() || is_default_shell_title(title) {
        "Terminal"
    } else {
        title
    }
}

/// Whether a process or title names a shell rather than something a shell is
/// running. A shell sitting at its prompt is the absence of activity, so
/// surfaces that report "what is this terminal doing" say nothing at all
/// rather than saying `fish` on every idle card.
pub(crate) fn is_default_shell_title(title: &str) -> bool {
    let title = title.rsplit(['/', '\\']).next().unwrap_or(title);
    let title = title.to_ascii_lowercase();
    // Windows reports the executable, extension and all, so an idle prompt
    // arrives here as `pwsh.exe` and matches nothing -- which reads to the
    // caller as "this terminal is running a program called pwsh.exe".
    let title = title.strip_suffix(".exe").unwrap_or(title.as_str());
    matches!(
        title,
        "zsh"
            | "bash"
            | "sh"
            | "dash"
            | "ksh"
            | "csh"
            | "tcsh"
            | "fish"
            | "nu"
            | "nushell"
            | "elvish"
            | "xonsh"
            | "pwsh"
            | "powershell"
            | "cmd"
    )
}

#[cfg(test)]
mod default_shell_title_tests {
    use super::is_default_shell_title;

    /// A shell sitting at its prompt is not "running" anything worth naming.
    #[test]
    fn a_bare_shell_is_not_a_running_program() {
        for shell in ["zsh", "bash", "fish", "nu", "pwsh", "cmd", "dash", "tcsh"] {
            assert!(is_default_shell_title(shell), "{shell}");
        }
    }

    /// Windows reports the executable, extension and all. Without stripping it
    /// an idle prompt was announcing itself as a program called `pwsh.exe`.
    #[test]
    fn windows_executables_are_recognised_with_their_extension() {
        for shell in ["pwsh.exe", "powershell.exe", "cmd.exe", "CMD.EXE"] {
            assert!(is_default_shell_title(shell), "{shell}");
        }
    }

    #[test]
    fn a_full_path_is_matched_on_its_last_component() {
        assert!(is_default_shell_title("/bin/zsh"));
        assert!(is_default_shell_title(
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
        ));
    }

    /// The point of all this: something the user actually started must survive.
    #[test]
    fn an_actual_program_is_still_named() {
        for program in ["vim", "htop", "node", "cargo", "claude", "zshrc", "bashful"] {
            assert!(!is_default_shell_title(program), "{program}");
        }
    }
}
