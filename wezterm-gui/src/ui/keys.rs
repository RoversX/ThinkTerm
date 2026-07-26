//! Platform-normalized text-editing modifiers.
//!
//! Every text surface in the app needs the same two questions answered: is this
//! the "command" chord (clipboard, select-all, line-start/end) and is this the
//! "word" chord (word-wise motion and deletion)? The answer differs per OS:
//!
//! | | command | word |
//! |---|---|---|
//! | macOS | ⌘ | ⌥ |
//! | Windows / Linux | Ctrl | Ctrl |
//!
//! Deriving that inline is how the surfaces drifted apart — some ended up
//! macOS-only, so Ctrl+A/C/V and Ctrl+arrow did nothing on Windows and Linux.
//! Resolve it here once and share the result.

/// Text-editing intent of a key event's modifiers, normalized across platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EditModifiers {
    /// Clipboard / select-all / line-start-end chord (⌘ on macOS, Ctrl elsewhere).
    pub command: bool,
    /// Word-wise motion and deletion chord (⌥ on macOS, Ctrl elsewhere).
    pub word: bool,
    /// Selection should be extended rather than replaced.
    pub shift: bool,
    /// Whether a modifier that rules out plain text input was held. Separate
    /// from the two roles above because "no chord I recognize" is not the same
    /// as "no modifier at all".
    blocked: bool,
}

/// Modifiers that mean "this key is a shortcut, not text". Mirrors the rule the
/// terminal input path already uses for the inline tab renamer
/// (`termwindow/keyevent.rs`), including LEADER and the positional variants —
/// a chord this type does not recognize must still reach the application.
///
/// AltGr is deliberately absent: X11 reports it through the keymap (Mod5 /
/// ISO_Level3_Shift) rather than as Ctrl+Alt, so composed characters arrive
/// with no modifier bits set and type normally.
pub(crate) const TEXT_MOD_BLOCKERS: window::Modifiers = window::Modifiers::ALT
    .union(window::Modifiers::CTRL)
    .union(window::Modifiers::SUPER)
    .union(window::Modifiers::LEADER)
    .union(window::Modifiers::LEFT_ALT)
    .union(window::Modifiers::RIGHT_ALT)
    .union(window::Modifiers::LEFT_CTRL)
    .union(window::Modifiers::RIGHT_CTRL);

impl EditModifiers {
    pub(crate) fn from_flags(shift: bool, ctrl: bool, alt: bool, super_: bool) -> Self {
        let macos = cfg!(target_os = "macos");
        // On Windows/Linux both roles live on Ctrl; the caller disambiguates by
        // key (arrows/Backspace are word-wise, letters are commands).
        let command = if macos {
            super_ && !alt && !ctrl
        } else {
            ctrl && !alt && !super_
        };
        let word = if macos {
            alt && !super_ && !ctrl
        } else {
            ctrl && !super_ && !alt
        };
        Self {
            command,
            word,
            shift,
            blocked: ctrl || alt || super_,
        }
    }

    /// The event carries no modifier that rules out text input, so it belongs
    /// to the focused field: characters are inserted, arrows and Backspace move
    /// or edit the caret.
    ///
    /// Anything looser would swallow application shortcuts. Ctrl+F on macOS and
    /// Super+F on Windows/Linux form neither chord above, but they are still
    /// shortcuts — the field must not eat the `f`.
    pub(crate) fn plain(self) -> bool {
        !self.blocked
    }
}

/// `wezterm_term::KeyModifiers` is a re-export of this same type, so this one
/// impl covers both the terminal-side and window-side key paths.
impl From<window::Modifiers> for EditModifiers {
    fn from(mods: window::Modifiers) -> Self {
        use window::Modifiers as M;
        let mut resolved = Self::from_flags(
            mods.contains(M::SHIFT),
            mods.contains(M::CTRL),
            mods.contains(M::ALT),
            mods.contains(M::SUPER),
        );
        // `from_flags` only sees the four canonical bits; LEADER and the
        // positional variants must block text input too.
        resolved.blocked |= mods.intersects(TEXT_MOD_BLOCKERS);
        resolved
    }
}

#[cfg(test)]
mod tests {
    use super::EditModifiers;

    const MACOS: bool = cfg!(target_os = "macos");

    fn m(shift: bool, ctrl: bool, alt: bool, super_: bool) -> EditModifiers {
        EditModifiers::from_flags(shift, ctrl, alt, super_)
    }

    #[test]
    fn command_chord_follows_platform() {
        // ⌘ alone is the command chord on macOS only.
        assert_eq!(m(false, false, false, true).command, MACOS);
        // Ctrl alone is the command chord everywhere else.
        assert_eq!(m(false, true, false, false).command, !MACOS);
    }

    #[test]
    fn word_chord_follows_platform() {
        // ⌥ alone is the word chord on macOS only.
        assert_eq!(m(false, false, true, false).word, MACOS);
        // Ctrl alone doubles as the word chord off macOS.
        assert_eq!(m(false, true, false, false).word, !MACOS);
    }

    #[test]
    fn mixed_chords_are_rejected() {
        // A second modifier means the user is asking for something else; never
        // claim the event (this is what keeps app-level chords working).
        for (shift, ctrl, alt, super_) in [
            (false, true, true, false),
            (false, true, false, true),
            (false, false, true, true),
            (false, true, true, true),
        ] {
            let mods = m(shift, ctrl, alt, super_);
            assert!(!mods.command, "command claimed {ctrl}/{alt}/{super_}");
            assert!(!mods.word, "word claimed {ctrl}/{alt}/{super_}");
        }
    }

    #[test]
    fn shortcut_modifiers_never_reach_the_field() {
        // The exact combinations that used to be swallowed: on its own platform
        // each of these forms neither chord, but they are still shortcuts. The
        // field must not eat the key — not the arrow, and not the character.
        for (label, mods) in [
            ("ctrl", m(false, true, false, false)),
            ("alt", m(false, false, true, false)),
            ("super", m(false, false, false, true)),
            ("ctrl+alt", m(false, true, true, false)),
        ] {
            if mods.command || mods.word {
                continue; // this one *is* a chord on this platform
            }
            assert!(!mods.plain(), "{label} must not be treated as plain input");
        }
    }

    #[test]
    fn plain_ignores_shift() {
        // Shift is how selections are extended and capitals are typed; it never
        // takes a key away from the field.
        assert!(m(false, false, false, false).plain());
        assert!(m(true, false, false, false).plain());
    }

    #[test]
    fn leader_and_positional_modifiers_block_text() {
        // `from_flags` only sees the four canonical bits, so the `Modifiers`
        // conversion has to catch LEADER and the left/right variants as well.
        use window::Modifiers as M;
        for mods in [
            M::LEADER,
            M::LEFT_ALT,
            M::RIGHT_ALT,
            M::LEFT_CTRL,
            M::RIGHT_CTRL,
        ] {
            assert!(
                !EditModifiers::from(mods).plain(),
                "{mods:?} must block text input"
            );
        }
        assert!(EditModifiers::from(M::NONE).plain());
        assert!(EditModifiers::from(M::SHIFT).plain());
    }
}
