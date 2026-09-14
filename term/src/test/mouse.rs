use crate::{
    color::ColorPalette, KeyModifiers, MouseButton, MouseEvent, MouseEventKind, Terminal,
    TerminalConfiguration, TerminalSize,
};
use std::io::Write;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Config;

impl TerminalConfiguration for Config {
    fn scrollback_size(&self) -> usize {
        24
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

struct Output(mpsc::Sender<Vec<u8>>);

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.send(bytes.to_vec()).unwrap();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn terminal() -> (Terminal, mpsc::Receiver<Vec<u8>>) {
    let (tx, rx) = mpsc::channel();
    let term = Terminal::new(
        TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 480,
            dpi: 96,
        },
        Arc::new(Config),
        "ThinkTerm",
        "test",
        Box::new(Output(tx)),
    );
    (term, rx)
}

fn wheel(term: &mut Terminal, button: MouseButton) {
    term.mouse_event(MouseEvent {
        kind: MouseEventKind::Press,
        button,
        x: 0,
        y: 0,
        x_pixel_offset: 0,
        y_pixel_offset: 0,
        modifiers: KeyModifiers::NONE,
    })
    .unwrap();
}

fn drain(term: &mut Terminal, output: &mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
    // An ordered DSR reply proves that all prior output has crossed the
    // threaded writer, including when there should be no wheel output.
    term.advance_bytes(b"\x1b[5n");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\x1b[0n") {
        bytes.extend(
            output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("terminal output barrier"),
        );
    }
    bytes.truncate(bytes.len() - 4);
    bytes
}

#[test]
fn zero_wheel_events_emit_neither_cursor_keys_nor_mouse_reports() {
    for mode in ["", "\x1b[?1000h", "\x1b[?1000h\x1b[?1006h"] {
        let (mut term, output) = terminal();
        term.advance_bytes(format!("\x1b[?1049h\x1b[?1h{}", mode));
        for _ in 0..4 {
            for button in [
                MouseButton::WheelUp(0),
                MouseButton::WheelDown(0),
                MouseButton::WheelLeft(0),
                MouseButton::WheelRight(0),
            ] {
                wheel(&mut term, button);
            }
        }
        assert_eq!(drain(&mut term, &output), b"", "mode {:?}", mode);
    }
}

#[test]
fn whole_wheel_events_keep_their_direction_and_scroll_speed() {
    let (mut term, output) = terminal();
    term.advance_bytes(b"\x1b[?1049h\x1b[?1h");
    wheel(&mut term, MouseButton::WheelUp(1));
    // The touchpad's fractional tail must not reverse an upward scroll.
    for _ in 0..4 {
        wheel(&mut term, MouseButton::WheelDown(0));
    }
    assert_eq!(drain(&mut term, &output), b"\x1bOA".repeat(3));
    wheel(&mut term, MouseButton::WheelDown(1));
    assert_eq!(drain(&mut term, &output), b"\x1bOB".repeat(3));

    term.advance_bytes(b"\x1b[?1000h\x1b[?1006h");
    wheel(&mut term, MouseButton::WheelUp(1));
    wheel(&mut term, MouseButton::WheelDown(1));
    assert_eq!(drain(&mut term, &output), b"\x1b[<64;1;1M\x1b[<65;1;1M");
}
