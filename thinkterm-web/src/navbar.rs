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

/// The bar on a phone, in points: room for one row of 11 pt capsules.
/// The desktop's 47 is a third of a phone's terminal at a large font,
/// and the grid pays for it in whole rows, so it came to four rows
/// (65 pt) at 11 pt -- a bar taller than the tab strip above it.
pub const MOBILE_NAV_CSS: f64 = 28.0;

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

/// Where a pane's bar starts, in CSS px from the canvas's top. A pane on
/// the tab's top row starts at the window's edge, over the padding, like
/// one at its left edge. Below a divider the bar starts right under the
/// line, which sits in the middle of the gap row, as the desktop's does.
pub fn bar_top(frame_top: usize, ch: f64, pad_top: f64) -> f64 {
    if frame_top == 0 {
        0.0
    } else {
        pad_top + frame_top as f64 * ch - ch / 2.0 + 1.0
    }
}

/// The rows of the grid on a desktop-shaped page `height_dev` tall, with
/// a bar `nav_dev` tall over every pane. The top pane's rows start the
/// half-cell pad `pad_dev` under its bar (`rows_offset_css`), and the bar
/// covers the pad above the grid, so the rows are what fits under bar and
/// pad -- less `px_dev`, the pixel a bar below a divider starts under its
/// line (`bar_top`) -- plus the rows each pane gives up for its bar
/// (`nav_rows`). Counting the bar in whole rows and a pad at the bottom
/// as well left a row or two blank under every pane.
pub fn grid_rows(height_dev: f64, nav_dev: f64, cell_h_dev: f64, pad_dev: f64, px_dev: f64) -> usize {
    if cell_h_dev <= 0.0 {
        return 1;
    }
    let under = ((height_dev - nav_dev - pad_dev - px_dev) / cell_h_dev).floor().max(1.0) as usize;
    under + nav_rows(nav_dev, cell_h_dev)
}

/// How far below its frame's top a pane's rows start under a bar
/// `nav_css` tall, in CSS px: the half-cell pad under the bar, as the
/// desktop draws them (`render/pane.rs`: the bar at the pane's top
/// without the window padding, the rows `pane_nav_height` below it with
/// the padding). The bar starts above the frame, so the rows start less
/// than a bar and a pad below it, and the part of a row the pane's rows
/// leave is under the last one, where the desktop leaves it too.
pub fn rows_offset_css(frame_top: usize, ch: f64, pad_top: f64, nav_css: f64) -> f64 {
    (bar_top(frame_top, ch, pad_top) + nav_css - frame_top as f64 * ch).max(0.0)
}

/// Where a bar goes, in CSS px from the canvas's top-left.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct NavRect {
    #[serde(rename = "pane")]
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
/// grid starts, `width_css` the canvas's width, and `rows_below` how far
/// below its frame's top a pane's rows start: the bar reaches down to
/// them, less `gap_css`, the pad the desktop leaves between the two.
pub fn rects(
    layout: &TabLayout,
    cell_css: (f64, f64),
    pad_css: (f64, f64),
    width_css: f64,
    gap_css: f64,
    rows_below: impl Fn(&PanePlacement) -> f64,
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
            let top = bar_top(p.frame.top, ch, pad_css.1);
            NavRect {
                pane_id: p.pane_id,
                left,
                top,
                width: right - left,
                height: pad_css.1 + p.frame.top as f64 * ch + rows_below(p) - gap_css - top,
            }
        })
        .collect()
}

/// A pane's title as the desktop shows it (`ui/mod.rs`
/// `terminal_title_for_display`): a bare shell at its prompt is
/// "Terminal", and a leading busy marker (the spinner some agents put in
/// the title) is lifted off into the `busy` flag.
pub fn display_title(title: &str) -> (String, bool) {
    let trimmed = title.trim_start();
    let (title, busy) = match trimmed.chars().next() {
        Some(ch) if is_busy_marker(ch) => (trimmed[ch.len_utf8()..].trim_start(), true),
        _ => (trimmed, false),
    };
    let title = title.trim();
    if title.is_empty() || is_default_shell_title(title) {
        (thinkterm_i18n::tr("web-title-terminal"), busy)
    } else {
        (title.to_string(), busy)
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
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct NavView {
    pub rect: NavRect,
    pub members: Vec<CapsuleView>,
    pub focused: bool,
    pub zoomed: bool,
    /// The close button was pressed once and waits for the press that means it.
    pub closing: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CapsuleView {
    #[serde(rename = "pane")]
    pub pane_id: thinkterm_proto::PaneId,
    pub title: String,
    pub busy: bool,
    pub current: bool,
    /// The card heading it (`tab_icons`); none with tab icons off.
    pub icon: Option<String>,
}


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
        let r = rects(&layout, (10.0, 20.0), (0.0, 0.0), 800.0, 0.0, |_| 47.0);
        assert_eq!((r[0].left, r[0].width), (0.0, 405.0), "to the divider's middle");
        assert_eq!((r[1].left, r[1].width), (405.0, 395.0), "from it to the tab's edge");
        assert_eq!(r[0].height, 47.0);
        // With the grid padded in a wider canvas, the outer bars still
        // reach the window's edges; the inner edge moves with the grid.
        // The bar reaches down over the padding to the rows.
        let r = rects(&layout, (10.0, 20.0), (10.0, 10.0), 1000.0, 0.0, |_| 47.0);
        assert_eq!((r[0].left, r[0].top, r[0].width, r[0].height), (0.0, 0.0, 415.0, 57.0));
        assert_eq!((r[1].left, r[1].width), (415.0, 585.0));
    }

    #[test]
    fn the_grid_has_the_rows_that_fit_under_the_bar() {
        // 800 px, a 47 px bar and a 10 px pad under it, 20 px rows: 37 rows
        // fit, and the pane gives up 3 for the bar. Half-cell pads above
        // and below and a whole-row bar made it 39 - 3 = 36.
        let (ch, pad) = (20.0, 10.0);
        assert_eq!(grid_rows(800.0, 47.0, ch, pad, 1.0), 40);
        let under = grid_rows(800.0, 47.0, ch, pad, 1.0) - nav_rows(47.0, ch);
        let end = 47.0 + pad + under as f64 * ch;
        assert!(end <= 800.0 && 800.0 - end < ch, "{end}");
        // A lower pane's bar starts a pixel under its divider's line; its
        // rows still end on the canvas.
        let rows = grid_rows(767.0, 47.0, ch, pad, 1.0);
        let top = 20usize;
        let start = pad + top as f64 * ch + rows_offset_css(top, ch, pad, 47.0);
        let end = start + (rows - top - nav_rows(47.0, ch)) as f64 * ch;
        assert!(end <= 767.0, "{end}");
    }

    #[test]
    fn the_rows_start_a_pad_under_the_bar_as_on_the_desktop() {
        // A pane on the tab's top row: its bar covers the half-cell
        // padding and the rows start that pad under the bar, a bar below
        // where the frame starts.
        assert_eq!(rows_offset_css(0, 22.0, 11.0, 47.0), 47.0);
        // Below a divider the bar starts a pixel under the line, half a
        // cell above the frame, and the rows a pad under the bar.
        assert_eq!(rows_offset_css(14, 22.0, 11.0, 47.0), 48.0);
        // Either way the bar is its own height, whatever rows the pane
        // gave up for it (three of 22px here).
        let mut lower = place(2, 0, 80, 13, 10);
        lower.frame.top = 14;
        let layout = TabLayout {
            tab_id: 1,
            window_id: 0,
            workspace: String::new(),
            cols: 80,
            rows: 27,
            size: TerminalSize::default(),
            panes: vec![place(1, 0, 80, 13, 10), lower],
            dividers: vec![],
            zoomed: None,
            hidden: vec![],
        };
        let r = rects(&layout, (10.0, 22.0), (10.0, 11.0), 820.0, 11.0, |p| rows_offset_css(p.frame.top, 22.0, 11.0, 47.0));
        assert_eq!((r[0].top, r[0].height), (0.0, 47.0));
        assert_eq!((r[1].top, r[1].height), (309.0, 47.0));
    }

    #[test]
    fn titles_read_like_the_desktop_s() {
        let dt = |s: &str| {
            let (title, busy) = display_title(s);
            (title, busy)
        };
        let t = |s: &str, b: bool| (s.to_string(), b);
        assert_eq!(dt("zsh"), t("Terminal", false));
        assert_eq!(dt("/usr/bin/fish"), t("Terminal", false));
        assert_eq!(dt("pwsh.exe"), t("Terminal", false));
        assert_eq!(dt("vim notes.md"), t("vim notes.md", false));
        assert_eq!(dt("\u{25d1} claude"), t("claude", true));
        assert_eq!(dt("\u{2733} claude"), t("\u{2733} claude", false), "the idle marker stays");
        assert_eq!(dt("  "), t("Terminal", false));
    }
}
