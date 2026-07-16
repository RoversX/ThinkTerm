pub mod context_menu;
pub mod icons;
pub mod platform_chrome;
pub mod right_sidebar;
pub mod sidebar;
pub mod status_icon;
pub mod tokens;

use crate::utilsprites::RenderMetrics;

pub use right_sidebar::{right_sidebar_file_preview_width, right_sidebar_width_for_metrics};
pub use sidebar::workspace_sidebar_width_for_metrics;

pub fn pane_nav_bar_height_for_metrics(render_metrics: RenderMetrics) -> usize {
    // Keep the bar proportional to the DPI-scaled UI font. Fixed physical
    // pixel clamps made it twice as tall in points on non-Retina displays.
    let cell_height = render_metrics.cell_size.height.max(1) as usize;
    (cell_height * 11 / 5).max(cell_height)
}

pub fn terminal_title_for_display(title: &str) -> &str {
    let title = title.trim();
    if title.is_empty() || is_default_shell_title(title) {
        "Terminal"
    } else {
        title
    }
}

fn is_default_shell_title(title: &str) -> bool {
    let title = title.rsplit(['/', '\\']).next().unwrap_or(title);
    matches!(
        title.to_ascii_lowercase().as_str(),
        "zsh"
            | "bash"
            | "sh"
            | "fish"
            | "nu"
            | "nushell"
            | "elvish"
            | "xonsh"
            | "pwsh"
            | "powershell"
    )
}
