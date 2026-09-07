//! DOM keyboard events to the terminal's key model. The browser already
//! applied the layout and Shift to `key`, so a printable key becomes
//! `KeyCode::Char` as typed and Shift is not reported for it, which is
//! what the desktop does after `normalize_shift_to_upper_case`.

use termwiz::input::{KeyCode, Modifiers};

/// What a `keydown` event carries that matters here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomKey<'a> {
    pub key: &'a str,
    pub code: &'a str,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    /// `KeyboardEvent.isComposing`: the IME owns this key.
    pub composing: bool,
}

pub fn map_key(k: &DomKey<'_>) -> Option<(KeyCode, Modifiers)> {
    if k.composing {
        return None;
    }
    let mut mods = Modifiers::NONE;
    if k.ctrl {
        mods |= Modifiers::CTRL;
    }
    if k.alt {
        mods |= Modifiers::ALT;
    }
    if k.meta {
        mods |= Modifiers::SUPER;
    }
    let with_shift = |code: KeyCode, mods: Modifiers| {
        Some((code, if k.shift { mods | Modifiers::SHIFT } else { mods }))
    };
    let code = match k.key {
        "Enter" => KeyCode::Enter,
        "Backspace" => KeyCode::Backspace,
        "Tab" => KeyCode::Tab,
        "Escape" => KeyCode::Escape,
        "ArrowUp" => KeyCode::UpArrow,
        "ArrowDown" => KeyCode::DownArrow,
        "ArrowLeft" => KeyCode::LeftArrow,
        "ArrowRight" => KeyCode::RightArrow,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Insert" => KeyCode::Insert,
        "Delete" => KeyCode::Delete,
        "Clear" => KeyCode::Clear,
        "Pause" => KeyCode::Pause,
        "PrintScreen" => KeyCode::PrintScreen,
        "ContextMenu" => KeyCode::Applications,
        // Modifier keys on their own, dead keys, and keys the IME is
        // handling do not reach the terminal.
        "Shift" | "Control" | "Alt" | "Meta" | "CapsLock" | "NumLock" | "ScrollLock" | "Dead"
        | "Process" | "Unidentified" | "AltGraph" | "Fn" | "Hyper" | "Super" | "OS" => {
            return None
        }
        other => {
            if let Some(n) = other.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
                if (1..=24).contains(&n) && other.len() <= 3 {
                    return with_shift(KeyCode::Function(n), mods);
                }
            }
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => {
                    if let Some(digit) = k.code.strip_prefix("Numpad") {
                        if let Ok(d) = digit.parse::<u8>() {
                            if d < 10 {
                                let numpad = match d {
                                    0 => KeyCode::Numpad0,
                                    1 => KeyCode::Numpad1,
                                    2 => KeyCode::Numpad2,
                                    3 => KeyCode::Numpad3,
                                    4 => KeyCode::Numpad4,
                                    5 => KeyCode::Numpad5,
                                    6 => KeyCode::Numpad6,
                                    7 => KeyCode::Numpad7,
                                    8 => KeyCode::Numpad8,
                                    _ => KeyCode::Numpad9,
                                };
                                return with_shift(numpad, mods);
                            }
                        }
                    }
                    // Shift is already folded into the character. Ctrl+Alt
                    // together with a non-alphanumeric character is AltGr
                    // on Windows: the character is what was typed.
                    if k.ctrl && k.alt && !c.is_ascii_alphanumeric() {
                        return Some((KeyCode::Char(c), mods - Modifiers::CTRL - Modifiers::ALT));
                    }
                    return Some((KeyCode::Char(c), mods));
                }
                _ => return None,
            }
        }
    };
    with_shift(code, mods)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: &str, code: &str) -> DomKey<'static> {
        DomKey {
            key: Box::leak(key.to_string().into_boxed_str()),
            code: Box::leak(code.to_string().into_boxed_str()),
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
            composing: false,
        }
    }

    #[test]
    fn printable_keys_carry_their_character_without_shift() {
        let mut k = key("A", "KeyA");
        k.shift = true;
        assert_eq!(map_key(&k), Some((KeyCode::Char('A'), Modifiers::NONE)));
        let mut k = key("c", "KeyC");
        k.ctrl = true;
        assert_eq!(map_key(&k), Some((KeyCode::Char('c'), Modifiers::CTRL)));
    }

    #[test]
    fn special_keys_keep_shift_and_the_function_row_is_numbered() {
        let mut k = key("ArrowUp", "ArrowUp");
        k.shift = true;
        assert_eq!(map_key(&k), Some((KeyCode::UpArrow, Modifiers::SHIFT)));
        assert_eq!(map_key(&key("F12", "F12")), Some((KeyCode::Function(12), Modifiers::NONE)));
        assert_eq!(map_key(&key("Enter", "NumpadEnter")), Some((KeyCode::Enter, Modifiers::NONE)));
        assert_eq!(map_key(&key("7", "Numpad7")), Some((KeyCode::Numpad7, Modifiers::NONE)));
    }

    #[test]
    fn altgr_characters_arrive_as_typed() {
        let mut k = key("@", "KeyQ");
        k.ctrl = true;
        k.alt = true;
        assert_eq!(map_key(&k), Some((KeyCode::Char('@'), Modifiers::NONE)));
        let mut k = key("x", "KeyX");
        k.ctrl = true;
        k.alt = true;
        assert_eq!(map_key(&k), Some((KeyCode::Char('x'), Modifiers::CTRL | Modifiers::ALT)));
    }

    #[test]
    fn modifiers_dead_keys_and_ime_keys_do_not_reach_the_terminal() {
        assert_eq!(map_key(&key("Shift", "ShiftLeft")), None);
        assert_eq!(map_key(&key("Dead", "Quote")), None);
        let mut k = key("a", "KeyA");
        k.composing = true;
        assert_eq!(map_key(&k), None);
    }
}
