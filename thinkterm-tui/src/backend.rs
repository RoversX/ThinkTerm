use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};
use std::io;
use std::time::Duration;
use termwiz::cell::{AttributeChange, Blink, Intensity, Underline};
use termwiz::color::{AnsiColor, ColorAttribute, SrgbaTuple};
use termwiz::input::InputEvent;
use termwiz::surface::{Change, CursorVisibility, Position as TermwizPosition};
use termwiz::terminal::buffered::BufferedTerminal;
use termwiz::terminal::{ScreenSize, Terminal, TerminalWaker};

/// Everything about a cell that decides how it is painted, so two neighbours
/// can be compared in one go rather than nine.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Pen {
    fg: Color,
    bg: Color,
    modifier: Modifier,
}

impl Pen {
    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            modifier: cell.modifier,
        }
    }
}

/// Ratatui output over ThinkTerm's existing Termwiz version.  Keeping this
/// adapter local avoids a second terminal/input type system: mux panes already
/// consume Termwiz keys, mouse events and paste data directly.
pub struct TermwizBackend<T: Terminal> {
    terminal: BufferedTerminal<T>,
}

impl<T: Terminal> TermwizBackend<T> {
    pub fn new(mut terminal: T) -> anyhow::Result<Self> {
        terminal.set_raw_mode()?;
        terminal.enter_alternate_screen()?;
        Ok(Self {
            terminal: BufferedTerminal::new(terminal)?,
        })
    }

    pub fn poll_input(&mut self, wait: Option<Duration>) -> anyhow::Result<Option<InputEvent>> {
        Ok(self.terminal.terminal().poll_input(wait)?)
    }

    pub fn check_for_resize(&mut self) -> anyhow::Result<bool> {
        Ok(self.terminal.check_for_resize()?)
    }

    pub fn screen_size(&mut self) -> anyhow::Result<ScreenSize> {
        Ok(self.terminal.terminal().get_screen_size()?)
    }

    pub fn waker(&mut self) -> TerminalWaker {
        self.terminal.terminal().waker()
    }

    /// Re-send the complete buffered surface. Ratatui and Termwiz both keep a
    /// local model, so an out-of-band host-terminal wrap or width disagreement
    /// otherwise survives indefinitely because neither model sees a diff.
    pub fn repaint(&mut self) -> anyhow::Result<()> {
        Ok(self.terminal.repaint()?)
    }
}

fn contiguous_ascii_cursor(x: u16, y: u16, symbol: &str, columns: usize) -> Option<(u16, u16)> {
    let [byte] = symbol.as_bytes() else {
        return None;
    };
    if !byte.is_ascii_graphic() && *byte != b' ' {
        return None;
    }
    let next = x.saturating_add(1);
    // Never predict through the right margin. Terminals disagree over whether
    // the cursor is already wrapped or is in a pending-wrap state there.
    (next < columns.min(u16::MAX as usize) as u16).then_some((next, y))
}

impl<T: Terminal> Backend for TermwizBackend<T> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        // Ratatui hands over the cells that changed, in reading order, and the
        // common case is a run of neighbours that share a style. Restating the
        // cursor and all nine attributes for each of them costs far more bytes
        // than the text does — on a phone reached over ssh, that is the
        // difference between a repaint that keeps up with a finger and one that
        // does not. Emit each only where it actually differs.
        let mut pen: Option<Pen> = None;
        let mut cursor: Option<(u16, u16)> = None;
        let (columns, _) = self.terminal.dimensions();
        for (x, y, cell) in content {
            if cursor != Some((x, y)) {
                self.terminal.add_change(Change::CursorPosition {
                    x: TermwizPosition::Absolute(x as usize),
                    y: TermwizPosition::Absolute(y as usize),
                });
            }
            let next = Pen::of(cell);
            let changed = pen != Some(next);
            pen = Some(next);
            cursor = contiguous_ascii_cursor(x, y, cell.symbol(), columns);
            if !changed {
                self.terminal
                    .add_change(Change::Text(cell.symbol().to_string()));
                continue;
            }
            self.terminal
                .add_change(Change::Attribute(AttributeChange::Foreground(
                    to_termwiz_color(cell.fg),
                )));
            self.terminal
                .add_change(Change::Attribute(AttributeChange::Background(
                    to_termwiz_color(cell.bg),
                )));
            self.terminal
                .add_change(Change::Attribute(AttributeChange::Intensity(
                    if cell.modifier.contains(Modifier::BOLD) {
                        Intensity::Bold
                    } else if cell.modifier.contains(Modifier::DIM) {
                        Intensity::Half
                    } else {
                        Intensity::Normal
                    },
                )));
            self.terminal
                .add_change(Change::Attribute(AttributeChange::Italic(
                    cell.modifier.contains(Modifier::ITALIC),
                )));
            self.terminal
                .add_change(Change::Attribute(AttributeChange::Underline(
                    if cell.modifier.contains(Modifier::UNDERLINED) {
                        Underline::Single
                    } else {
                        Underline::None
                    },
                )));
            self.terminal
                .add_change(Change::Attribute(AttributeChange::Reverse(
                    cell.modifier.contains(Modifier::REVERSED),
                )));
            self.terminal
                .add_change(Change::Attribute(AttributeChange::Invisible(
                    cell.modifier.contains(Modifier::HIDDEN),
                )));
            self.terminal
                .add_change(Change::Attribute(AttributeChange::StrikeThrough(
                    cell.modifier.contains(Modifier::CROSSED_OUT),
                )));
            self.terminal
                .add_change(Change::Attribute(AttributeChange::Blink(
                    if cell.modifier.contains(Modifier::RAPID_BLINK) {
                        Blink::Rapid
                    } else if cell.modifier.contains(Modifier::SLOW_BLINK) {
                        Blink::Slow
                    } else {
                        Blink::None
                    },
                )));
            self.terminal
                .add_change(Change::Text(cell.symbol().to_string()));
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.terminal
            .add_change(Change::CursorVisibility(CursorVisibility::Hidden));
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.terminal
            .add_change(Change::CursorVisibility(CursorVisibility::Visible));
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        let (x, y) = self.terminal.cursor_position();
        Ok(Position::new(saturating_u16(x), saturating_u16(y)))
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.terminal.add_change(Change::CursorPosition {
            x: TermwizPosition::Absolute(position.x as usize),
            y: TermwizPosition::Absolute(position.y as usize),
        });
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.terminal
            .add_change(Change::ClearScreen(ColorAttribute::Default));
        Ok(())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        if clear_type == ClearType::All {
            self.clear()
        } else {
            Err(io::Error::other(format!(
                "Termwiz does not support clearing {clear_type:?}"
            )))
        }
    }

    fn size(&self) -> io::Result<Size> {
        let (cols, rows) = self.terminal.dimensions();
        Ok(Size::new(saturating_u16(cols), saturating_u16(rows)))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        let size = self
            .terminal
            .terminal()
            .get_screen_size()
            .map_err(io::Error::other)?;
        Ok(WindowSize {
            columns_rows: Size::new(saturating_u16(size.cols), saturating_u16(size.rows)),
            pixels: Size::new(saturating_u16(size.xpixel), saturating_u16(size.ypixel)),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.terminal.flush().map_err(io::Error::other)
    }
}

fn saturating_u16(value: usize) -> u16 {
    value.min(u16::MAX as usize) as u16
}

fn to_termwiz_color(color: Color) -> ColorAttribute {
    match color {
        Color::Reset => ColorAttribute::Default,
        Color::Black => AnsiColor::Black.into(),
        Color::DarkGray => AnsiColor::Grey.into(),
        Color::Gray => AnsiColor::Silver.into(),
        Color::Red => AnsiColor::Maroon.into(),
        Color::LightRed => AnsiColor::Red.into(),
        Color::Green => AnsiColor::Green.into(),
        Color::LightGreen => AnsiColor::Lime.into(),
        Color::Yellow => AnsiColor::Olive.into(),
        Color::LightYellow => AnsiColor::Yellow.into(),
        Color::Blue => AnsiColor::Navy.into(),
        Color::LightBlue => AnsiColor::Blue.into(),
        Color::Magenta => AnsiColor::Purple.into(),
        Color::LightMagenta => AnsiColor::Fuchsia.into(),
        Color::Cyan => AnsiColor::Teal.into(),
        Color::LightCyan => AnsiColor::Aqua.into(),
        Color::White => AnsiColor::White.into(),
        Color::Indexed(index) => ColorAttribute::PaletteIndex(index),
        Color::Rgb(red, green, blue) => ColorAttribute::TrueColorWithPaletteFallback(
            SrgbaTuple::from((red, green, blue)),
            nearest_xterm_256(red, green, blue),
        ),
    }
}

/// Keep RGB colors meaningful when the host advertises only `xterm-256color`.
/// A default fallback collapses every RGB color to the same terminal color.
fn nearest_xterm_256(red: u8, green: u8, blue: u8) -> u8 {
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

    fn distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
        let red = i32::from(a.0) - i32::from(b.0);
        let green = i32::from(a.1) - i32::from(b.1);
        let blue = i32::from(a.2) - i32::from(b.2);
        (red * red + green * green + blue * blue) as u32
    }

    fn nearest_level(value: u8) -> usize {
        CUBE.iter()
            .enumerate()
            .min_by_key(|(_, level)| (i16::from(value) - i16::from(**level)).abs())
            .map_or(0, |(index, _)| index)
    }

    let red_level = nearest_level(red);
    let green_level = nearest_level(green);
    let blue_level = nearest_level(blue);
    let cube_rgb = (CUBE[red_level], CUBE[green_level], CUBE[blue_level]);
    let cube_index = 16 + 36 * red_level + 6 * green_level + blue_level;

    let average = (u16::from(red) + u16::from(green) + u16::from(blue)) / 3;
    let gray_level = ((i32::from(average) - 8 + 5) / 10).clamp(0, 23) as usize;
    let gray = 8 + 10 * gray_level as u8;
    if distance((red, green, blue), (gray, gray, gray)) < distance((red, green, blue), cube_rgb) {
        (232 + gray_level) as u8
    } else {
        cube_index as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_colors_have_a_256_color_fallback() {
        assert_eq!(nearest_xterm_256(255, 0, 0), 196);
        assert_eq!(nearest_xterm_256(128, 128, 128), 244);
        assert!(matches!(
            to_termwiz_color(Color::Rgb(137, 180, 250)),
            ColorAttribute::TrueColorWithPaletteFallback(_, _)
        ));
    }

    #[test]
    fn cursor_elision_is_limited_to_safe_ascii_before_the_margin() {
        assert_eq!(contiguous_ascii_cursor(4, 2, "x", 20), Some((5, 2)));
        assert_eq!(contiguous_ascii_cursor(4, 2, " ", 20), Some((5, 2)));
        assert_eq!(contiguous_ascii_cursor(19, 2, "x", 20), None);
        assert_eq!(contiguous_ascii_cursor(4, 2, "界", 20), None);
        assert_eq!(contiguous_ascii_cursor(4, 2, "e\u{301}", 20), None);
    }
}
