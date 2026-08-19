//! Staged probes for the wasm build. Each entry point does strictly more than
//! the last, so a failure identifies which layer is not portable rather than
//! just reporting that something panicked.

use std::sync::Arc;
use std::sync::Mutex;

// wasm32-unknown-unknown has nowhere to print a panic, and the trap that
// follows tells you only "unreachable". Stash the message where JS can read it
// out of linear memory after catching the trap.
static PANIC: Mutex<String> = Mutex::new(String::new());
static mut PANIC_BUF: [u8; 1024] = [0; 1024];

#[no_mangle]
pub extern "C" fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        if let Ok(mut slot) = PANIC.lock() {
            *slot = info.to_string();
        }
    }));
}

#[no_mangle]
pub extern "C" fn panic_msg_ptr() -> *const u8 {
    let msg = PANIC.lock().map(|m| m.clone()).unwrap_or_default();
    let bytes = msg.as_bytes();
    let n = bytes.len().min(1024);
    unsafe {
        let buf = (&raw mut PANIC_BUF) as *mut u8;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n);
        buf as *const u8
    }
}

#[no_mangle]
pub extern "C" fn panic_msg_len() -> u32 {
    PANIC.lock().map(|m| m.len().min(1024) as u32).unwrap_or(0)
}
use termwiz::escape::parser::Parser;
use wezterm_term::{Terminal, TerminalConfiguration, TerminalSize};

#[derive(Debug)]
struct Config;

impl TerminalConfiguration for Config {
    fn color_palette(&self) -> wezterm_term::color::ColorPalette {
        Default::default()
    }
}

fn size() -> TerminalSize {
    TerminalSize {
        rows: 24,
        cols: 80,
        pixel_width: 800,
        pixel_height: 480,
        dpi: 96,
    }
}

#[no_mangle]
pub extern "C" fn stage1_alloc() -> u32 {
    let v: Vec<u8> = (0..64).collect();
    v.len() as u32
}

#[no_mangle]
pub extern "C" fn stage2_parser() -> u32 {
    let mut p = Parser::new();
    let mut n = 0u32;
    p.parse(b"hello\x1b[1;31m wasm\x1b[0m", |_| n += 1);
    n
}

#[no_mangle]
pub extern "C" fn stage3_palette() -> u32 {
    let pal = Config.color_palette();
    pal.colors.0.len() as u32
}

#[no_mangle]
pub extern "C" fn stage4_terminal() -> u32 {
    let term = Terminal::new(size(), Arc::new(Config), "probe", "0.1", Box::new(Vec::new()));
    term.screen().physical_rows as u32
}

#[no_mangle]
pub extern "C" fn stage5_advance() -> u32 {
    let mut term = Terminal::new(size(), Arc::new(Config), "probe", "0.1", Box::new(Vec::new()));
    term.advance_bytes(b"hello\x1b[1;31m wasm\x1b[0m");
    let mut out = String::new();
    term.screen().for_each_phys_line(|idx, line| {
        if idx == 0 {
            out = line.as_str().trim_end().to_string();
        }
    });
    if out == "hello wasm" {
        0
    } else {
        1
    }
}

/// A CSI parameter is a u32; on a 32-bit usize `cursor.x + n as usize` in the
/// ECH handler overflows before .min() can clamp it. Any program writing to the
/// pty can send this.
#[no_mangle]
pub extern "C" fn stage6_ech_overflow() -> u32 {
    let mut term = Terminal::new(size(), Arc::new(Config), "probe", "0.1", Box::new(Vec::new()));
    term.advance_bytes(b"abcdef\x1b[3G\x1b[4294967295X");
    let mut out = String::new();
    term.screen().for_each_phys_line(|idx, line| {
        if idx == 0 {
            out = line.as_str().trim_end().to_string();
        }
    });
    out.len() as u32
}
