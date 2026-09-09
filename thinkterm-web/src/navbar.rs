//! The strip above each pane: the desktop's pane nav bar
//! (`wezterm-gui/src/termwindow/render/pane.rs`), as HTML laid over the
//! canvas. The geometry is pure and tested natively; the DOM half only
//! builds on wasm.

use crate::layout::{PanePlacement, TabLayout};

/// The desktop's bar in CSS px: the header font's cell times 9/4. With
/// the default 14 pt header font on a 2x Mac that is 94 device px, which
/// is also what the desktop's listings show it reserving (three rows of
/// a 46.67 px cell, the smallest count a 94 px bar needs).
pub const DESKTOP_NAV_CSS: f64 = 47.0;

/// The bar's height on this page. The desktop's, scaled by how far the
/// page's cell is from the desktop's: with the font matched the ratio
/// is one and the bar is the desktop's to the pixel.
pub fn nav_css(page_cell_css: f64, desktop_cell_css: Option<f64>) -> f64 {
    match desktop_cell_css {
        Some(d) if d > 0.0 && page_cell_css > 0.0 => DESKTOP_NAV_CSS * page_cell_css / d,
        _ => DESKTOP_NAV_CSS,
    }
}

/// Rows of a frame the bar takes from the pane, as the desktop counts
/// them (`resize.rs` `terminal_size_for_positioned_pane`): whatever is
/// left below the bar is divided into whole cells.
pub fn nav_rows(nav_dev: f64, cell_h_dev: f64) -> usize {
    if cell_h_dev <= 0.0 {
        return 0;
    }
    (nav_dev / cell_h_dev).ceil() as usize
}

/// How far below its frame's top a pane's content starts, in device
/// px: the bar, but never more than the frame has spare over the pane's
/// own rows. A layout nobody with a bar has laid out (a CLI split) has
/// no spare rows; the bar then sits over the first rows until the next
/// claim from here reshapes the panes with room for it.
pub fn content_offset_dev(place: &PanePlacement, nav_dev: f32, cell_h_dev: f32) -> f32 {
    let spare = place.frame.rows.saturating_sub(place.content.1) as f32 * cell_h_dev;
    nav_dev.min(spare)
}

/// Where a bar goes, in CSS px from the canvas's top-left.
#[derive(Debug, Clone, PartialEq)]
pub struct NavRect {
    pub pane_id: thinkterm_proto::PaneId,
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

/// One bar per drawn pane, spanning the pane's chrome span like the
/// desktop's: to the window's edge where the pane touches the tab's
/// edge (past the padding the grid sits in), half a cell into the
/// divider where it does not, so neighbours tile. `pad_css` is where the
/// grid starts and `width_css` the canvas's width.
pub fn rects(
    layout: &TabLayout,
    cell_css: (f64, f64),
    nav_css: f64,
    pad_css: (f64, f64),
    width_css: f64,
) -> Vec<NavRect> {
    let (cw, ch) = cell_css;
    layout
        .panes
        .iter()
        .map(|p| {
            let left = if p.frame.left == 0 { 0.0 } else { pad_css.0 + p.frame.left as f64 * cw - cw / 2.0 };
            let end = p.frame.left + p.frame.cols;
            let right = if end >= layout.cols {
                width_css.max(pad_css.0 + layout.cols as f64 * cw)
            } else {
                pad_css.0 + end as f64 * cw + cw / 2.0
            };
            // A pane on the tab's top row starts at the window's edge,
            // over the padding, like one at its left edge.
            let top = if p.frame.top == 0 { 0.0 } else { pad_css.1 + p.frame.top as f64 * ch };
            NavRect {
                pane_id: p.pane_id,
                left,
                top,
                width: right - left,
                height: nav_css + (pad_css.1 + p.frame.top as f64 * ch - top),
            }
        })
        .collect()
}

/// A pane's title as the desktop shows it (`ui/mod.rs`
/// `terminal_title_for_display`): a bare shell at its prompt is
/// "Terminal", and a leading busy marker (the spinner some agents put in
/// the title) is lifted off into the `busy` flag.
pub fn display_title(title: &str) -> (&str, bool) {
    let trimmed = title.trim_start();
    let (title, busy) = match trimmed.chars().next() {
        Some(ch) if is_busy_marker(ch) => (trimmed[ch.len_utf8()..].trim_start(), true),
        _ => (trimmed, false),
    };
    let title = title.trim();
    if title.is_empty() || is_default_shell_title(title) {
        ("Terminal", busy)
    } else {
        (title, busy)
    }
}

fn is_busy_marker(ch: char) -> bool {
    // Braille and the half-circle frames Claude Code spins; U+2733 is
    // its idle marker and deliberately not here.
    matches!(ch as u32, 0x2800..=0x28ff | 0x25d0..=0x25d3 | 0xf0130 | 0xf0a9e..=0xf0aa5 | 0xee00..=0xee0b)
}

fn is_default_shell_title(title: &str) -> bool {
    let title = title.rsplit(['/', '\\']).next().unwrap_or(title).to_ascii_lowercase();
    let title = title.strip_suffix(".exe").unwrap_or(&title);
    matches!(
        title,
        "zsh" | "bash" | "sh" | "dash" | "ksh" | "csh" | "tcsh" | "fish" | "nu" | "nushell" | "elvish"
            | "xonsh" | "pwsh" | "powershell" | "cmd"
    )
}

/// What one bar shows.
#[derive(Debug, Clone, PartialEq)]
pub struct NavView {
    pub rect: NavRect,
    pub members: Vec<CapsuleView>,
    pub focused: bool,
    pub zoomed: bool,
    /// The close button was pressed once and waits for the press that means it.
    pub closing: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CapsuleView {
    pub pane_id: thinkterm_proto::PaneId,
    pub title: String,
    pub busy: bool,
    pub current: bool,
}

#[cfg(target_arch = "wasm32")]
mod dom {
    use super::NavView;

    pub struct NavBars {
        root: web_sys::Element,
    }

    fn escape(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        for c in text.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                c => out.push(c),
            }
        }
        out
    }

    impl NavBars {
        pub fn mount(id: &str) -> Option<Self> {
            let root = web_sys::window()?.document()?.get_element_by_id(id)?;
            Some(Self { root })
        }

        pub fn element(&self) -> &web_sys::Element {
            &self.root
        }

        /// The markup for every bar. The caller keeps the last string and
        /// only sets it when it changed: layouts are re-listed often and
        /// mostly unchanged.
        pub fn html(bars: &[NavView]) -> String {
            let icon = crate::icons::svg;
            let mut html = String::new();
            for bar in bars {
                let class = if bar.focused { "nav focused" } else { "nav" };
                html.push_str(&format!(
                    "<div class=\"{class}\" data-nav=\"{}\" style=\"left:{:.2}px;top:{:.2}px;width:{:.2}px;height:{:.2}px\"><span class=\"caps\">",
                    bar.rect.pane_id, bar.rect.left, bar.rect.top, bar.rect.width, bar.rect.height
                ));
                for m in &bar.members {
                    let class = if m.current { "cap current" } else { "cap" };
                    let glyph = if m.busy { format!("<span class=\"spin\">{}</span>", icon("loader-circle")) } else { icon("square-terminal").to_string() };
                    let (close_class, close_body) = if bar.closing && m.current {
                        ("x danger", "close?")
                    } else {
                        ("x", icon("x"))
                    };
                    html.push_str(&format!(
                        "<span class=\"{class}\" data-pane=\"{}\" title=\"{}\">{glyph}<span class=\"t\">{}</span><span class=\"{close_class}\" data-action=\"close-pane\" data-pane=\"{}\" title=\"Close this pane and end its program. Asks twice.\">{close_body}</span></span>",
                        m.pane_id, escape(&m.title), escape(&m.title), m.pane_id
                    ));
                }
                html.push_str("</span><span class=\"acts\">");
                html.push_str(&format!("<span class=\"act\" data-action=\"new-tab\" title=\"New tab\">{}</span>", icon("plus")));
                html.push_str(&format!("<span class=\"act\" data-action=\"split-below\" title=\"Split down\">{}</span>", icon("square-split-vertical")));
                html.push_str(&format!("<span class=\"act\" data-action=\"split-right\" title=\"Split right\">{}</span>", icon("square-split-horizontal")));
                let (zoom_icon, zoom_title) = if bar.zoomed { ("minimize-2", "Unzoom") } else { ("maximize-2", "Zoom this pane to the whole tab") };
                html.push_str(&format!("<span class=\"act\" data-action=\"zoom\" title=\"{zoom_title}\">{}</span>", icon(zoom_icon)));
                html.push_str("</span></div>");
            }
            html
        }

        pub fn set(&self, html: &str) {
            self.root.set_inner_html(html);
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub use dom::NavBars;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Rect, StackMember};
    use wezterm_term::TerminalSize;

    fn place(id: usize, left: usize, cols: usize, rows: usize, content_rows: usize) -> PanePlacement {
        PanePlacement {
            pane_id: id,
            tab_id: 1,
            window_id: 0,
            frame: Rect { left, top: 0, cols, rows },
            content: (cols, content_rows),
            is_active: false,
            is_zoomed: false,
            title: String::new(),
            alt_screen: false,
            physical_top: 0,
            size: TerminalSize::default(),
            workspace: String::new(),
            stack: vec![StackMember { pane_id: id, title: String::new() }],
        }
    }

    #[test]
    fn the_bar_takes_the_desktop_s_rows_of_a_frame() {
        // 94 device px over a 46.67 px cell: three rows, like the desktop.
        assert_eq!(nav_rows(94.0, 46.67), 3);
        assert_eq!(nav_rows(46.0, 23.0), 2);
        assert_eq!(nav_rows(47.0, 23.0), 3);
        assert_eq!(nav_rows(47.0, 0.0), 0);
        assert_eq!(nav_css(23.0, Some(23.333)), 47.0 * 23.0 / 23.333);
        assert_eq!(nav_css(23.0, None), 47.0);
    }

    #[test]
    fn content_sits_below_the_bar_but_never_past_the_spare_rows() {
        let laid_out = place(1, 0, 80, 45, 42);
        assert_eq!(content_offset_dev(&laid_out, 94.0, 46.67), 94.0);
        let cli = place(1, 0, 80, 45, 45);
        assert_eq!(content_offset_dev(&cli, 94.0, 46.67), 0.0, "no room: the bar overlaps");
    }

    #[test]
    fn bars_tile_across_the_divider_and_reach_the_tab_s_edges() {
        let layout = TabLayout {
            tab_id: 1,
            window_id: 0,
            workspace: String::new(),
            cols: 80,
            rows: 24,
            size: TerminalSize::default(),
            panes: vec![place(1, 0, 40, 24, 21), place(2, 41, 39, 24, 21)],
            dividers: vec![],
            zoomed: None,
            hidden: vec![],
        };
        let r = rects(&layout, (10.0, 20.0), 47.0, (0.0, 0.0), 800.0);
        assert_eq!((r[0].left, r[0].width), (0.0, 405.0), "to the divider's middle");
        assert_eq!((r[1].left, r[1].width), (405.0, 395.0), "from it to the tab's edge");
        assert_eq!(r[0].height, 47.0);
        // With the grid padded in a wider canvas, the outer bars still
        // reach the window's edges; the inner edge moves with the grid.
        let r = rects(&layout, (10.0, 20.0), 47.0, (10.0, 10.0), 1000.0);
        assert_eq!((r[0].left, r[0].top, r[0].width, r[0].height), (0.0, 0.0, 415.0, 57.0));
        assert_eq!((r[1].left, r[1].width), (415.0, 585.0));
    }

    #[test]
    fn titles_read_like_the_desktop_s() {
        assert_eq!(display_title("zsh"), ("Terminal", false));
        assert_eq!(display_title("/usr/bin/fish"), ("Terminal", false));
        assert_eq!(display_title("pwsh.exe"), ("Terminal", false));
        assert_eq!(display_title("vim notes.md"), ("vim notes.md", false));
        assert_eq!(display_title("\u{25d1} claude"), ("claude", true));
        assert_eq!(display_title("\u{2733} claude"), ("\u{2733} claude", false), "the idle marker stays");
        assert_eq!(display_title("  "), ("Terminal", false));
    }
}
