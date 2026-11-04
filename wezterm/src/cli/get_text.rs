use clap::{Parser, ValueEnum};
use codec::Pdu;
use mux::pane::PaneId;
use std::fmt::Write as _;
use std::io::Write;
use termwiz::cell::{CellAttributes, Intensity, Underline};
use termwiz::color::ColorAttribute;
use termwiz::surface::{Line, SEQ_ZERO};
use termwiz_funcs::lines_to_escapes;
use wezterm_client::client::Client;
use wezterm_term::color::{ColorPalette, SrgbaTuple};
use wezterm_term::{ScrollbackOrVisibleRowIndex, StableRowIndex};

#[derive(Debug, Parser, Clone)]
pub struct GetText {
    /// Specify the target pane.
    /// The default is to use the current pane based on the
    /// environment variable WEZTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// The pane by name rather than id: its label, its thread's name (a
    /// thread with one pane), or `thread/label`.
    #[arg(long, conflicts_with = "pane_id")]
    pane: Option<String>,

    /// The starting line number.
    /// 0 is the first line of terminal screen.
    /// Negative numbers proceed backwards into the scrollback.
    /// The default value is unspecified is 0, the first line of
    /// the terminal screen.
    #[arg(long, allow_hyphen_values = true)]
    start_line: Option<ScrollbackOrVisibleRowIndex>,

    /// The ending line number.
    /// 0 is the first line of terminal screen.
    /// Negative numbers proceed backwards into the scrollback.
    /// The default value if unspecified is the bottom of the
    /// the terminal screen.
    #[arg(long, allow_hyphen_values = true)]
    end_line: Option<ScrollbackOrVisibleRowIndex>,

    /// Include escape sequences that color and style the text: the same
    /// as `--format vt`. If omitted, unattributed text will be returned.
    #[arg(long)]
    escapes: bool,

    /// How to print it: "text", the default; "vt", with the escape
    /// sequences that color and style it; or "html", with its colors.
    #[arg(long, value_enum, conflicts_with = "escapes")]
    format: Option<TextFormat>,

    /// Leave out the spaces that end lines, and the empty lines at the end.
    #[arg(long)]
    trim: bool,

    /// Join the lines the terminal wrapped back into the lines they were.
    #[arg(long)]
    unwrap: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TextFormat {
    Text,
    Vt,
    Html,
}

impl GetText {
    fn text_format(&self) -> TextFormat {
        if self.escapes {
            TextFormat::Vt
        } else {
            self.format.unwrap_or(TextFormat::Text)
        }
    }

    /// Whether the server's palette is wanted: HTML spells its colors out.
    pub fn wants_palette(&self) -> bool {
        self.text_format() == TextFormat::Html
    }

    /// `pushes` carries what the server sent unasked, its palette among it,
    /// when `wants_palette`.
    pub async fn run(
        self,
        client: Client,
        pushes: Option<smol::channel::Receiver<Pdu>>,
    ) -> anyhow::Result<()> {
        if self
            .pane
            .as_deref()
            .is_some_and(crate::cli::reverse::looks_like_target)
            && self.text_format() != TextFormat::Text
        {
            return Err(
                crate::cli::UsageError("reverse access supports plain text only".into()).into(),
            );
        }
        if let Some(target) = crate::cli::reverse::resolve(&client, self.pane.as_deref()).await? {
            use thinkterm_reverse_access::protocol::{Command, ReadOptions, Reply};
            let (reply, payload) = target
                .router
                .request_read(
                    target.route,
                    Command::Read {
                        export: target.export,
                        options: ReadOptions {
                            start: self.start_line.map(|n| n as i64),
                            end: self.end_line.map(|n| n as i64),
                            trim: self.trim,
                            unwrap: self.unwrap,
                        },
                    },
                )
                .await?;
            match (reply, payload) {
                (Reply::Text { .. }, Some(text)) => {
                    std::io::stdout()
                        .lock()
                        .write_all(text.text().as_bytes())
                        .map_err(crate::stdout_error)?;
                    return Ok(());
                }
                (Reply::Failed(failure), _) => anyhow::bail!("reverse read failed: {:?}", failure),
                _ => anyhow::bail!("invalid reverse read response"),
            }
        }
        let (client, pane_id) = crate::cli::target::resolve_reaching_servers(
            client,
            self.pane_id,
            self.pane.as_deref(),
        )
        .await?;

        let info = client
            .get_dimensions(codec::GetPaneRenderableDimensions { pane_id })
            .await?;

        let start_line = match self.start_line {
            None => info.dimensions.physical_top,
            Some(n) if n >= 0 => info.dimensions.physical_top + n as StableRowIndex,
            Some(n) => {
                let line = info.dimensions.physical_top as isize + n as isize;
                if line < info.dimensions.scrollback_top as isize {
                    info.dimensions.scrollback_top
                } else {
                    line as StableRowIndex
                }
            }
        };

        let end_line = match self.end_line {
            None => info.dimensions.physical_top + info.dimensions.viewport_rows as StableRowIndex,
            Some(n) if n >= 0 => info.dimensions.physical_top + n as StableRowIndex,
            Some(n) => {
                let line = info.dimensions.physical_top as isize + n as isize;
                if line < info.dimensions.scrollback_top as isize {
                    info.dimensions.scrollback_top
                } else {
                    line as StableRowIndex
                }
            }
        };

        if start_line > end_line {
            return Err(
                crate::cli::UsageError("--start-line is after --end-line".to_string()).into(),
            );
        }

        let lines = client
            .get_lines(codec::GetLines {
                pane_id: pane_id.into(),
                lines: vec![start_line..end_line + 1],
            })
            .await?;

        let mut lines: Vec<Line> = lines
            .lines
            .extract_data()
            .0
            .into_iter()
            .map(|(_idx, line)| line)
            .collect();
        if self.unwrap {
            lines = unwrapped(lines);
        }
        if self.trim {
            lines = trimmed(lines);
        }

        let mut out = std::io::stdout().lock();
        let written = match self.text_format() {
            TextFormat::Text => lines
                .iter()
                .try_for_each(|line| writeln!(out, "{}", line.as_str())),
            TextFormat::Vt => writeln!(out, "{}", lines_to_escapes(lines)?),
            TextFormat::Html => {
                // The push came in ahead of the answers above.
                let palette = pushes
                    .and_then(|pushes| {
                        std::iter::from_fn(|| pushes.try_recv().ok()).find_map(|pdu| match pdu {
                            Pdu::DefaultPalette(codec::DefaultPalette { palette }) => Some(palette),
                            _ => None,
                        })
                    })
                    .unwrap_or_default();
                write!(out, "{}", lines_to_html(&lines, &palette))
            }
        };
        written.map_err(crate::stdout_error)
    }
}

/// The lines with each one the terminal wrapped joined to the next.
fn unwrapped(lines: Vec<Line>) -> Vec<Line> {
    let mut joined: Vec<Line> = Vec::with_capacity(lines.len());
    let mut continues = false;
    for line in lines {
        let wrapped = line.last_cell_was_wrapped();
        match joined.last_mut() {
            Some(last) if continues => last.append_line(line, SEQ_ZERO),
            _ => joined.push(line),
        }
        continues = wrapped;
    }
    joined
}

/// The lines without the blanks that end them, and without the empty lines
/// at the end.
fn trimmed(mut lines: Vec<Line>) -> Vec<Line> {
    for line in &mut lines {
        let keep = line
            .visible_cells()
            .filter(|cell| !cell.str().trim().is_empty())
            .last()
            .map(|cell| cell.cell_index() + cell.width())
            .unwrap_or(0);
        if keep < line.len() {
            line.resize(keep, SEQ_ZERO);
        }
    }
    while lines.last().is_some_and(|line| line.len() == 0) {
        lines.pop();
    }
    lines
}

/// The lines as a `<pre>` block, each run of cells styled alike in a span
/// that spells out its colors from `palette`.
fn lines_to_html(lines: &[Line], palette: &ColorPalette) -> String {
    let mut html = format!(
        "<pre style=\"color:{};background-color:{}\">",
        palette.foreground.to_rgb_string(),
        palette.background.to_rgb_string()
    );
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            html.push('\n');
        }
        let mut run = String::new();
        let mut run_style = String::new();
        for cell in line.visible_cells() {
            let style = cell_style(cell.attrs(), palette);
            if style != run_style {
                push_run(&mut html, &run, &run_style);
                run.clear();
                run_style = style;
            }
            run.push_str(cell.str());
        }
        push_run(&mut html, &run, &run_style);
    }
    html.push_str("</pre>\n");
    html
}

fn push_run(html: &mut String, text: &str, style: &str) {
    if text.is_empty() {
        return;
    }
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            c => escaped.push(c),
        }
    }
    if style.is_empty() {
        html.push_str(&escaped);
    } else {
        let _ = write!(html, "<span style=\"{style}\">{escaped}</span>");
    }
}

/// The color of text in `fg`: as the GUI and the browser client do by
/// default (`bold_brightens_ansi_colors`), bold lifts the first eight
/// palette colors to their bright twins.
fn text_color(attrs: &CellAttributes, fg: ColorAttribute, palette: &ColorPalette) -> SrgbaTuple {
    match fg {
        ColorAttribute::PaletteIndex(idx) if idx < 8 && attrs.intensity() == Intensity::Bold => {
            palette.resolve_fg(ColorAttribute::PaletteIndex(idx + 8))
        }
        other => palette.resolve_fg(other),
    }
}

/// CSS for a cell's look; empty for the plain default.
fn cell_style(attrs: &CellAttributes, palette: &ColorPalette) -> String {
    let mut rules = vec![];
    let (fg, bg) = (attrs.foreground(), attrs.background());
    if attrs.reverse() {
        rules.push(format!("color:{}", palette.resolve_bg(bg).to_rgb_string()));
        rules.push(format!(
            "background-color:{}",
            text_color(attrs, fg, palette).to_rgb_string()
        ));
    } else {
        if fg != ColorAttribute::Default {
            rules.push(format!(
                "color:{}",
                text_color(attrs, fg, palette).to_rgb_string()
            ));
        }
        if bg != ColorAttribute::Default {
            rules.push(format!(
                "background-color:{}",
                palette.resolve_bg(bg).to_rgb_string()
            ));
        }
    }
    match attrs.intensity() {
        Intensity::Bold => rules.push("font-weight:bold".to_string()),
        Intensity::Half => rules.push("opacity:0.6".to_string()),
        Intensity::Normal => {}
    }
    if attrs.italic() {
        rules.push("font-style:italic".to_string());
    }
    let underline = attrs.underline() != Underline::None;
    match (underline, attrs.strikethrough()) {
        (true, true) => rules.push("text-decoration:underline line-through".to_string()),
        (true, false) => rules.push("text-decoration:underline".to_string()),
        (false, true) => rules.push("text-decoration:line-through".to_string()),
        (false, false) => {}
    }
    if attrs.invisible() {
        rules.push("visibility:hidden".to_string());
    }
    rules.join(";")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use wezterm_term::{Terminal, TerminalSize};

    /// The lines a terminal of `cols` columns shows after `text`.
    fn screen(cols: usize, text: &str) -> Vec<Line> {
        let mut term = Terminal::new(
            TerminalSize {
                rows: 4,
                cols,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 0,
            },
            Arc::new(config::TermConfig::new()),
            "ThinkTerm",
            "test",
            Box::new(Vec::new()),
        );
        term.advance_bytes(text);
        let screen = term.screen();
        screen
            .lines_in_phys_range(0..screen.physical_rows)
            .into_iter()
            .collect()
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.as_str().into_owned())
            .collect()
    }

    #[test]
    fn wrapped_lines_join_back_and_trimming_drops_the_blank_ends() {
        let lines = screen(5, "abcdefgh\r\nxy  ");
        assert_eq!(texts(&trimmed(lines.clone())), ["abcde", "fgh", "xy"]);
        assert_eq!(texts(&trimmed(unwrapped(lines))), ["abcdefgh", "xy"]);
    }

    #[test]
    fn html_spells_out_colors_and_escapes_the_text() {
        let lines = screen(12, "\x1b[1;31m<b>\x1b[0m & x");
        let palette = ColorPalette::default();
        let html = lines_to_html(&trimmed(lines), &palette);
        let red = palette.colors.0[9].to_rgb_string();
        assert!(html.starts_with("<pre style=\"color:"), "{}", html);
        assert!(
            html.contains(&format!(
                "<span style=\"color:{red};font-weight:bold\">&lt;b&gt;</span>"
            )),
            "{}",
            html
        );
        assert!(html.contains(" &amp; x</pre>"), "{}", html);

        let lines = screen(12, "\x1b[31mred");
        let dim_red = palette.colors.0[1].to_rgb_string();
        let html = lines_to_html(&trimmed(lines), &palette);
        assert!(
            html.contains(&format!("<span style=\"color:{dim_red}\">red</span>")),
            "{}",
            html
        );
    }
}
