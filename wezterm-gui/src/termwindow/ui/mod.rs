pub mod icons;
pub mod sidebar;
pub mod status_icon;
pub mod tokens;

use crate::utilsprites::RenderMetrics;

pub use sidebar::workspace_sidebar_width_for_metrics;

pub fn pane_nav_bar_height_for_metrics(render_metrics: RenderMetrics) -> usize {
    (render_metrics.cell_size.height as usize + tokens::PANE_NAV_EXTRA_HEIGHT)
        .clamp(tokens::PANE_NAV_MIN_HEIGHT, tokens::PANE_NAV_MAX_HEIGHT)
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
