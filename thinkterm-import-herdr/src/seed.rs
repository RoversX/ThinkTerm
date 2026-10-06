use crate::model::Runtime;

pub(crate) fn seed_ansi(runtime: &Runtime) -> String {
    let mut ansi = String::new();
    if let Some(input) = &runtime.input_state {
        for (mode, enabled) in [
            (1049, input.alternate_screen),
            (1, input.application_cursor),
            (2004, input.bracketed_paste),
            (1004, input.focus_reporting),
            (1007, input.mouse_alternate_scroll),
            (2031, input.color_scheme_reporting),
        ] {
            ansi.push_str(&format!(
                "\x1b[?{}{}",
                mode,
                if enabled { 'h' } else { 'l' }
            ));
        }
        let mouse = match input.mouse_protocol_mode.as_str() {
            "Press" => 9,
            "PressRelease" => 1000,
            "ButtonMotion" => 1002,
            "AnyMotion" => 1003,
            _ => 0,
        };
        for mode in [9, 1000, 1002, 1003] {
            ansi.push_str(&format!(
                "\x1b[?{}{}",
                mode,
                if mode == mouse { 'h' } else { 'l' }
            ));
        }
        let encoding = match input.mouse_protocol_encoding.as_str() {
            "Utf8" => 1005,
            "Sgr" => 1006,
            "SgrPixels" => 1016,
            _ => 0,
        };
        for mode in [1005, 1006, 1016] {
            ansi.push_str(&format!("\x1b[?{}l", mode));
        }
        if encoding != 0 {
            ansi.push_str(&format!("\x1b[?{}h", encoding));
        }
        if input.modify_other_keys {
            ansi.push_str("\x1b[>4;2m");
        }
    }
    if let Some(protocol) = &runtime.keyboard_protocol_ansi {
        ansi.push_str(protocol);
    } else if runtime.keyboard_protocol_flags != 0 {
        ansi.push_str(&format!("\x1b[>{}u", runtime.keyboard_protocol_flags));
    }
    if let Some(history) = &runtime.initial_history_ansi {
        ansi.push_str(history);
    }
    ansi
}
