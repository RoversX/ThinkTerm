//! The fields of a plugin's panel, as this window edits them: what each
//! holds -- taken from the player, and handed back with every change --
//! where its text was laid out when last painted, and the keys, presses and
//! text that edit it. Editing never waits for the plugin.
//!
//! Where the keyboard is, the player decides (`thinkterm_plugin_panel`):
//! a panel has it only once the user pressed in one of its fields, or used
//! the key they chose for it, and gives it back on Escape, a press on the
//! terminal, or going off show. While it has it, what the field with it does
//! not use goes to the player, which sends on the keys the panel takes; the
//! rest are the app's, and never the terminal's. Lost without the user --
//! the plugin let go, the field went, the plugin stopped -- the keyboard
//! goes nowhere until they press Escape or somewhere: what they type next
//! was meant for the panel.

use super::{spend, PanelFonts, PluginPanel, Shown, Status, LAYER};
use crate::termwindow::{RightSidebarMode, TermWindow};
use crate::ui::{EditModifiers, TextInputState, UiPalette};
use crate::utilsprites::RenderMetrics;
use config::keyassignment::{ClipboardCopyDestination, ClipboardPasteSource};
use std::borrow::Cow;
use std::rc::Rc;
use termwiz::input::{KeyCode as TermKeyCode, Modifiers as TermModifiers};
use thinkterm_plugin_channel::wire::{PanelRequest, Raw};
use thinkterm_plugin_panel::player::{field_clear_at, field_inside, field_limit, FIELD_ICON_GAP};
use thinkterm_plugin_panel::{Field, FieldKind, Font, Keyboard, Mods};
use wezterm_font::LoadedFont;
use window::{
    DeadKeyStatus, MouseButtons, MouseCursor, MouseEvent, MouseEventKind as WMEK, MousePress,
    Point, Rect, RectF, WindowOps,
};

/// Where the tokens of the text a field shows the system start: apart from
/// the Note editor's, so that an edit meant for one is never the other's.
static SNAPSHOT_TOKENS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 63);
/// The caret's width, in the UI's units.
const CARET_WIDTH: f32 = 2.0;
/// How much of a field's text, about its caret, the system is shown.
const SNAPSHOT_BYTES: usize = 4096;
/// How thick the outline of a panel with the keyboard is, in the UI's units.
const RING_WIDTH: f32 = 2.0;

/// A field's text as it is edited in this window.
pub(super) struct Editor {
    /// The player's revision of the text this was taken from: taken again
    /// when that moves, the plugin having put text there.
    revision: u32,
    input: TextInputState,
    /// In window pixels: on one line, how far the text is scrolled across;
    /// in one of lines, how far down.
    scroll: f32,
    /// Whether the next layout scrolls the caret into view: after it moved,
    /// and not after the wheel moved the lines.
    follow: bool,
    /// The x a caret going up and down keeps to, from the lines' left.
    goal: Option<f32>,
    layout: Option<FieldLayout>,
}

impl Editor {
    fn new(text: &str, revision: u32) -> Self {
        let mut input = TextInputState::new();
        input.set_text_end(text.to_string());
        Self {
            revision,
            input,
            scroll: 0.0,
            follow: true,
            goal: None,
            layout: None,
        }
    }
}

/// Where a field's text went when it was last painted, in window pixels.
struct FieldLayout {
    /// Where a line's x of 0 is, and the first line's top: scrolled.
    left: f32,
    top: f32,
    line_height: f32,
    /// The box the text shows in.
    inner: RectF,
    lines: Vec<LaidLine>,
    /// Where each line of what the field holds starts, by character.
    starts: Vec<usize>,
}

/// A line as it is laid in the box: the characters it shows, by index in
/// what the field holds, which of the field's lines they are of, and where
/// each boundary from `start` to `end` is, from the line's left.
struct LaidLine {
    start: usize,
    end: usize,
    of: usize,
    xs: Vec<f32>,
}

impl Shown {
    /// Brings the editors in line with the player's fields: one for each,
    /// holding the player's text when the plugin put text there, and none
    /// for a field gone.
    pub(super) fn sync_editors(&mut self) {
        let Self {
            player,
            editors,
            edits,
            ..
        } = self;
        editors.retain(|id, _| player.field(id).is_some());
        for field in player.fields() {
            let Some((text, revision)) = player.field_text(&field.id) else {
                continue;
            };
            if editors
                .get(&field.id)
                .is_some_and(|editor| editor.revision == revision)
            {
                continue;
            }
            editors.insert(field.id.clone(), Editor::new(text, revision));
            *edits += 1;
        }
    }

    /// Tells the plugin what the player decided to: where the keyboard
    /// went, what a field holds, a key.
    pub(super) fn tell(&mut self) {
        for input in self.player.told() {
            let input = Raw::new(&input);
            super::plugins::tell_panel(self.view, PanelRequest::Input { input });
        }
    }

    /// The host no longer serves it: the keyboard goes back, and the plugin,
    /// whose view is gone, is told nothing of it.
    pub(super) fn let_go_of_keyboard(&mut self) {
        self.player.blur();
        self.player.told();
    }

    /// Where the pointer at `x`, `y` in window pixels is, in the view's units,
    /// even outside it.
    fn units(&self, x: f32, y: f32) -> (f32, f32) {
        (
            (x - self.origin.0) / self.scale,
            (y - self.origin.1) / self.scale,
        )
    }

    /// Whether field `id` holds something, as it is edited here.
    pub(super) fn editors_hold(&self, id: &str) -> bool {
        self.editors
            .get(id)
            .is_some_and(|editor| !editor.input.text().is_empty())
    }

    /// The field whose button that empties it is under the pointer at `x`,
    /// `y` in window pixels: the end of a field that has one and holds
    /// something, all of its height.
    pub(super) fn clear_under(&self, x: f32, y: f32) -> Option<String> {
        let (x, y) = self.units(x, y);
        let field = self.player.field_at(x, y)?;
        let (left, _, _) = field_clear_at(field)?;
        let over = x >= left - FIELD_ICON_GAP / 2.0 && x < field.x + field.w;
        (over && self.editors_hold(&field.id)).then(|| field.id.clone())
    }

    /// Where `field`'s text shows, in window pixels.
    fn inner(&self, field: &Field) -> RectF {
        let inside = field_inside(field);
        euclid::rect(
            self.origin.0 + inside.left * self.scale,
            self.origin.1 + inside.top * self.scale,
            inside.width() * self.scale,
            inside.height() * self.scale,
        )
    }
}

/// The text of the field with the keyboard as the system was last shown it
/// (`set_native_text_input_snapshot`), for its press-and-hold accents,
/// dictation and reconversion: whose it is, what it was made from -- the
/// view's edits, and where the text was laid out -- and the token the
/// system's edits of it come back with.
pub(crate) struct FieldSnapshot {
    view: u64,
    field: String,
    edits: u64,
    place: [u32; 7],
    token: u64,
}

impl PluginPanel {
    /// The panel, or its extended view.
    fn view_mut(&mut self, extended: bool) -> Option<&mut Shown> {
        if extended {
            self.extended.as_mut().map(|(extended, _)| extended)
        } else {
            Some(&mut self.shown)
        }
    }

    /// The view with the keyboard, and whether it is the extended one.
    fn keyboard_view(&mut self) -> Option<(&mut Shown, bool)> {
        let extended = self
            .extended
            .as_ref()
            .is_some_and(|(extended, _)| extended.player.has_keyboard());
        let shown = self.view_mut(extended)?;
        shown.player.has_keyboard().then_some((shown, extended))
    }
}

/// What a key did in a field.
enum Act {
    /// The caret or the selection moved.
    Moved,
    /// What the field holds changed.
    Edited,
    Submit,
    Copy,
    Cut,
    Paste,
    /// Not the field's: for the player, by name, when it has one.
    Key(Option<String>),
}

impl TermWindow {
    /// Whether a view of the plugin panel on show has the keyboard -- or is
    /// to have it, the user having asked for the panel with their key before
    /// it opened.
    pub(crate) fn plugin_panel_has_keyboard(&self) -> bool {
        let asked = self.plugin_panel_focus_asked.as_deref().is_some_and(|asked| {
            matches!(self.right_sidebar_mode, RightSidebarMode::Plugin(id) if id.as_str() == asked)
        });
        asked
            || self
                .right_sidebar_plugin
                .as_ref()
                .is_some_and(PluginPanel::has_keyboard)
    }

    /// The keyboard goes back from the plugin panel: the user put it
    /// elsewhere.
    pub(crate) fn release_plugin_panel_keyboard(&mut self) {
        self.plugin_panel_focus_asked = None;
        self.plugin_panel_keys_held = false;
        self.forget_plugin_field_snapshot();
        let Some(panel) = self.right_sidebar_plugin.as_mut() else {
            return;
        };
        panel.selecting = None;
        for extended in [false, true] {
            if let Some(shown) = panel.view_mut(extended) {
                if shown.player.has_keyboard() {
                    shown.player.blur();
                    shown.tell();
                }
            }
        }
    }

    /// The key the user chose for plugin `plugin`'s panel: the sidebar opens
    /// on it, and it has the keyboard -- in its first field, once it has one.
    /// Pressed again while it has it, the keyboard goes back.
    pub(crate) fn focus_plugin_panel(&mut self, plugin: &str) {
        self.plugin_panel_keys_held = false;
        let on_it = |mode: RightSidebarMode| matches!(mode, RightSidebarMode::Plugin(id) if id.as_str() == plugin);
        if on_it(self.right_sidebar_mode)
            && !self.right_sidebar_collapsed
            && self.plugin_panel_has_keyboard()
        {
            self.release_plugin_panel_keyboard();
        } else {
            self.show_plugin_panel(plugin);
            if !on_it(self.right_sidebar_mode) || self.right_sidebar_collapsed {
                return;
            }
            self.clear_right_sidebar_text_focus();
            match self
                .right_sidebar_plugin
                .as_mut()
                .filter(|panel| panel.plugin == plugin)
            {
                Some(panel) => {
                    panel.shown.player.focus_panel();
                    panel.shown.tell();
                }
                // Opened as the window paints next, and given it then.
                None => self.plugin_panel_focus_asked = Some(plugin.to_string()),
            }
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// The plugin panel lost the keyboard without the user: while this
    /// window has it, what they press next is held from the terminal.
    pub(crate) fn hold_plugin_panel_keys(&mut self) {
        if self.focused.is_some() {
            self.plugin_panel_keys_held = true;
        }
    }

    /// A key pressed while keys are held from the terminal: nobody's, but
    /// for the app's own chords. Escape lets the terminal have the next.
    pub(crate) fn plugin_panel_held_key(&mut self, key: TermKeyCode, mods: TermModifiers) -> bool {
        let plain = EditModifiers::from(mods).plain();
        if plain && key == TermKeyCode::Escape {
            self.plugin_panel_keys_held = false;
        }
        plain
    }

    /// The window lost the keyboard, and the system's view of a field's
    /// text with it: shown again once it is back. A drag ended, and no key
    /// is held any more.
    pub(crate) fn plugin_panel_window_blurred(&mut self) {
        self.plugin_field_snapshot = None;
        self.plugin_panel_keys_held = false;
        if let Some(panel) = self.right_sidebar_plugin.as_mut() {
            panel.selecting = None;
        }
    }

    /// Any press or release in the window, before it goes where it goes. A
    /// press is the user's, whose keys are held no longer; a drag selecting
    /// in a field ends where the button comes up, in the panel or not.
    pub(crate) fn plugin_panel_saw_button(&mut self, kind: &WMEK) {
        if matches!(kind, WMEK::Press(_)) {
            self.plugin_panel_keys_held = false;
        }
        if matches!(kind, WMEK::Press(_) | WMEK::Release(_)) {
            if let Some(panel) = self.right_sidebar_plugin.as_mut() {
                panel.selecting = None;
            }
        }
    }

    /// A key pressed while a view of the plugin panel has the keyboard: the
    /// field with it edits, and what it does not use goes to the player. True
    /// when the panel took it; false for one it did not, which the app's own
    /// keys may have, and the terminal never.
    pub(crate) fn plugin_panel_key(&mut self, key: TermKeyCode, mods: TermModifiers) -> bool {
        let Some(mut panel) = self.right_sidebar_plugin.take() else {
            // Asked for, and not open yet: what is pressed meanwhile is
            // nobody's, and Escape takes the ask back.
            if self.plugin_panel_focus_asked.is_none() {
                return false;
            }
            if key == TermKeyCode::Escape {
                self.plugin_panel_focus_asked = None;
            }
            return EditModifiers::from(mods).plain();
        };
        let handled = self.plugin_panel_key_in(&mut panel, key, mods);
        self.right_sidebar_plugin = Some(panel);
        handled
    }

    fn plugin_panel_key_in(
        &mut self,
        panel: &mut PluginPanel,
        key: TermKeyCode,
        mods: TermModifiers,
    ) -> bool {
        let Some((shown, _)) = panel.keyboard_view() else {
            return false;
        };
        shown.sync_editors();
        let to_player = player_mods(mods);
        let Some(id) = shown.player.focused_field().map(str::to_string) else {
            let taken = key_name(&key).is_some_and(|name| shown.player.key(&name, to_player));
            shown.tell();
            return taken;
        };
        let Some(field) = shown.player.field(&id) else {
            return false;
        };
        let kind = field.kind;
        let limit = field_limit(field);
        let lines = kind == FieldKind::Lines;
        let secret = kind == FieldKind::Secret;
        let Some(editor) = shown.editors.get_mut(&id) else {
            return false;
        };
        let edit = EditModifiers::from(mods);
        let shift = edit.shift;
        let macos = cfg!(target_os = "macos");
        let len = editor.input.char_len();
        // Up and down by a line or a page keep to the x they started from.
        let keeps_goal = lines
            && edit.plain()
            && matches!(
                key,
                TermKeyCode::UpArrow
                    | TermKeyCode::DownArrow
                    | TermKeyCode::PageUp
                    | TermKeyCode::PageDown
            );

        let chord = if edit.command {
            match key {
                TermKeyCode::Char('a') | TermKeyCode::Char('A') => {
                    editor.input.caret_select_all();
                    Some(Act::Moved)
                }
                TermKeyCode::Char('c') | TermKeyCode::Char('C') => Some(Act::Copy),
                TermKeyCode::Char('x') | TermKeyCode::Char('X') => Some(Act::Cut),
                TermKeyCode::Char('v') | TermKeyCode::Char('V') => Some(Act::Paste),
                TermKeyCode::Enter if lines => Some(Act::Submit),
                // ⌘←, ⌘→ and ⌘⌫ are the line's start and end on a Mac, and
                // ⌘↑, ⌘↓ the whole text's; elsewhere Ctrl+Home and Ctrl+End.
                TermKeyCode::LeftArrow if macos => {
                    line_home(editor, shift);
                    Some(Act::Moved)
                }
                TermKeyCode::RightArrow if macos => {
                    line_end(editor, shift);
                    Some(Act::Moved)
                }
                TermKeyCode::UpArrow if macos && lines => {
                    editor.input.caret_set(0, shift);
                    Some(Act::Moved)
                }
                TermKeyCode::DownArrow if macos && lines => {
                    editor.input.caret_set(len, shift);
                    Some(Act::Moved)
                }
                TermKeyCode::Home if !macos => {
                    editor.input.caret_set(0, shift);
                    Some(Act::Moved)
                }
                TermKeyCode::End if !macos => {
                    editor.input.caret_set(len, shift);
                    Some(Act::Moved)
                }
                TermKeyCode::Backspace if macos => {
                    delete_to_line_start(editor);
                    Some(Act::Edited)
                }
                _ => None,
            }
        } else {
            None
        };
        // Word by word: ⌥ on a Mac, Ctrl elsewhere.
        let chord = chord.or_else(|| {
            if !edit.word {
                return None;
            }
            match key {
                TermKeyCode::LeftArrow => {
                    editor.input.caret_word_left(shift);
                    Some(Act::Moved)
                }
                TermKeyCode::RightArrow => {
                    editor.input.caret_word_right(shift);
                    Some(Act::Moved)
                }
                TermKeyCode::Backspace => {
                    editor.input.caret_delete_word_back();
                    Some(Act::Edited)
                }
                _ => None,
            }
        });
        let act = match chord {
            Some(act) => act,
            // Any chord the field does not use is the app's.
            None if !edit.plain() => return false,
            None => match key {
                TermKeyCode::LeftArrow => {
                    editor.input.caret_move_left(shift);
                    Act::Moved
                }
                TermKeyCode::RightArrow => {
                    editor.input.caret_move_right(shift);
                    Act::Moved
                }
                TermKeyCode::Home => {
                    line_home(editor, shift);
                    Act::Moved
                }
                TermKeyCode::End => {
                    line_end(editor, shift);
                    Act::Moved
                }
                TermKeyCode::UpArrow if lines => {
                    vertical(editor, -1, shift);
                    Act::Moved
                }
                TermKeyCode::DownArrow if lines => {
                    vertical(editor, 1, shift);
                    Act::Moved
                }
                TermKeyCode::PageUp if lines => {
                    let rows = page_rows(editor);
                    vertical(editor, -rows, shift);
                    Act::Moved
                }
                TermKeyCode::PageDown if lines => {
                    let rows = page_rows(editor);
                    vertical(editor, rows, shift);
                    Act::Moved
                }
                TermKeyCode::Delete => {
                    editor.input.caret_delete_forward();
                    Act::Edited
                }
                TermKeyCode::Backspace => {
                    editor.input.caret_backspace();
                    Act::Edited
                }
                TermKeyCode::Enter if lines => {
                    insert(&mut editor.input, "\n", limit, lines);
                    Act::Edited
                }
                TermKeyCode::Enter => Act::Submit,
                TermKeyCode::Char(ch) if !ch.is_control() => {
                    insert(&mut editor.input, &ch.to_string(), limit, lines);
                    Act::Edited
                }
                other => Act::Key(key_name(&other)),
            },
        };

        let taken = match act {
            Act::Moved | Act::Edited => {
                if !keeps_goal {
                    editor.goal = None;
                }
                editor.follow = true;
                shown.edits += 1;
                if matches!(act, Act::Edited) {
                    shown.player.edit_field(&id, editor.input.text());
                }
                true
            }
            Act::Submit => {
                shown.player.submit(&id);
                true
            }
            Act::Copy => {
                // Nothing leaves a secret field.
                if !secret {
                    let text = copied(&editor.input);
                    if !text.is_empty() {
                        self.copy_to_clipboard(ClipboardCopyDestination::Clipboard, text);
                    }
                }
                true
            }
            Act::Cut => {
                if !secret {
                    let text = match editor.input.caret_take_selected_text() {
                        Some(text) => text,
                        None => {
                            let text = editor.input.text().to_string();
                            editor.input.clear();
                            text
                        }
                    };
                    if !text.is_empty() {
                        self.copy_to_clipboard(ClipboardCopyDestination::Clipboard, text);
                    }
                    editor.follow = true;
                    shown.edits += 1;
                    shown.player.edit_field(&id, editor.input.text());
                }
                true
            }
            Act::Paste => {
                self.paste_into_right_sidebar_from_clipboard(ClipboardPasteSource::Clipboard);
                true
            }
            Act::Key(Some(name)) => shown.player.key(&name, to_player),
            Act::Key(None) => false,
        };
        shown.tell();
        taken
    }

    /// Text that arrived for the plugin panel with the keyboard -- composed,
    /// or pasted: put in the field with it. True when the panel has the
    /// keyboard, in a field or not: it goes nowhere else.
    pub(crate) fn plugin_panel_text(&mut self, text: &str) -> bool {
        let Some(panel) = self.right_sidebar_plugin.as_mut() else {
            return self.plugin_panel_focus_asked.is_some();
        };
        let Some((shown, _)) = panel.keyboard_view() else {
            return false;
        };
        shown.sync_editors();
        let Some(id) = shown.player.focused_field().map(str::to_string) else {
            return true;
        };
        let Some(field) = shown.player.field(&id) else {
            return true;
        };
        let (limit, lines) = (field_limit(field), field.kind == FieldKind::Lines);
        if let Some(editor) = shown.editors.get_mut(&id) {
            insert(&mut editor.input, text, limit, lines);
            editor.follow = true;
            editor.goal = None;
            shown.player.edit_field(&id, editor.input.text());
            shown.edits += 1;
        }
        shown.tell();
        true
    }

    /// Copies what is selected in the field with the keyboard, or all of
    /// it -- not out of a secret one.
    pub(crate) fn plugin_panel_copy(&mut self, destination: ClipboardCopyDestination) {
        let text = self.right_sidebar_plugin.as_mut().and_then(|panel| {
            let (shown, _) = panel.keyboard_view()?;
            let id = shown.player.focused_field()?;
            let field = shown.player.field(id)?;
            if field.kind == FieldKind::Secret {
                return None;
            }
            Some(copied(&shown.editors.get(id)?.input))
        });
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            self.copy_to_clipboard(destination, text);
        }
    }

    /// The selection in the field with the keyboard goes.
    pub(crate) fn plugin_panel_clear_selection(&mut self) {
        let Some(panel) = self.right_sidebar_plugin.as_mut() else {
            return;
        };
        let Some((shown, _)) = panel.keyboard_view() else {
            return;
        };
        let Some(id) = shown.player.focused_field().map(str::to_string) else {
            return;
        };
        if let Some(editor) = shown.editors.get_mut(&id) {
            editor.input.clear_selection();
            shown.edits += 1;
        }
    }

    /// Shows the system the text of view `shown`'s field with the keyboard,
    /// on a Mac, for what it does with text -- press and hold for an accent,
    /// dictation -- when it changed; or takes back what it showed of the view
    /// once no field of it has the keyboard. A secret field's is never shown.
    pub(super) fn sync_plugin_field_snapshot(&mut self, shown: &Shown) {
        if !cfg!(target_os = "macos") {
            return;
        }
        let focused = shown.player.focused_field().and_then(|id| {
            let field = shown.player.field(id)?;
            let editor = shown.editors.get(id)?;
            let layout = editor.layout.as_ref()?;
            (field.kind != FieldKind::Secret).then_some((id, editor, layout))
        });
        let Some((id, editor, layout)) = focused else {
            if self
                .plugin_field_snapshot
                .as_ref()
                .is_some_and(|held| held.view == shown.view)
            {
                self.forget_plugin_field_snapshot();
            }
            return;
        };
        // Every change to what a field holds, its caret or its scroll counts
        // as an edit of the view; a move, a new width or a new font moves
        // its text.
        let inner = layout.inner;
        let place = [
            layout.left,
            layout.top,
            layout.line_height,
            inner.min_x(),
            inner.min_y(),
            inner.width(),
            inner.height(),
        ]
        .map(f32::to_bits);
        if self.plugin_field_snapshot.as_ref().is_some_and(|held| {
            held.view == shown.view
                && held.field == id
                && held.edits == shown.edits
                && held.place == place
        }) {
            return;
        }
        let text = editor.input.text();
        let byte_of = |at: usize| text.char_indices().nth(at).map_or(text.len(), |(at, _)| at);
        let (from, to) = editor
            .input
            .caret_selection_range()
            .unwrap_or((editor.input.cursor, editor.input.cursor));
        let caret = byte_of(editor.input.cursor);
        let (mut selection, mut start, mut end) = (byte_of(from)..byte_of(to), 0, text.len());
        // Only the text about the caret, as the Note editor shows it: what
        // the system does with text wants no more, and a field of lines
        // holds tens of kilobytes.
        if text.len() > SNAPSHOT_BYTES {
            start = caret.saturating_sub(SNAPSHOT_BYTES / 2);
            end = (start + SNAPSHOT_BYTES).min(text.len());
            start = end.saturating_sub(SNAPSHOT_BYTES);
            while start > 0 && !text.is_char_boundary(start) {
                start -= 1;
            }
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
            if selection.start < start || selection.end > end {
                selection = caret..caret;
            }
        }
        // Each character of it, where it was laid out.
        let first = text[..start].chars().count();
        let bytes: Vec<usize> = text[start..end]
            .char_indices()
            .map(|(at, _)| at)
            .chain([end - start])
            .collect();
        let mut hits = Vec::new();
        for (row, line) in layout.lines.iter().enumerate() {
            if line.end < first || line.start > first + bytes.len() {
                continue;
            }
            let top = layout.top + row as f32 * layout.line_height;
            for (at, x) in line.xs.iter().enumerate() {
                let Some(byte) = (line.start + at)
                    .checked_sub(first)
                    .and_then(|at| bytes.get(at))
                else {
                    continue;
                };
                hits.push(window::NativeTextHit {
                    rect: Rect::new(
                        Point::new((layout.left + x) as isize, top as isize),
                        window::Size::new(1, layout.line_height.max(1.0) as isize),
                    ),
                    byte: *byte,
                });
            }
        }
        let token = SNAPSHOT_TOKENS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(window) = self.window.as_ref() {
            window.set_native_text_input_snapshot(Some(window::NativeTextInputSnapshot {
                token,
                revision: text_revision(text),
                source_base: start,
                text: text[start..end].to_string(),
                selection: selection.start - start..selection.end - start,
                hits,
            }));
        }
        self.plugin_field_snapshot = Some(FieldSnapshot {
            view: shown.view,
            field: id.to_string(),
            edits: shown.edits,
            place,
            token,
        });
    }

    /// Takes back the text of a field of view `view` the system was shown,
    /// if it was one of its: the view went.
    pub(crate) fn forget_plugin_field_snapshot_of(&mut self, view: u64) {
        if self
            .plugin_field_snapshot
            .as_ref()
            .is_some_and(|held| held.view == view)
        {
            self.forget_plugin_field_snapshot();
        }
    }

    /// Takes back the text of a plugin's field the system was shown.
    pub(crate) fn forget_plugin_field_snapshot(&mut self) {
        if self.plugin_field_snapshot.take().is_some() {
            if let Some(window) = self.window.as_ref() {
                window.set_native_text_input_snapshot(None);
            }
        }
    }

    /// The system edited the text of the field it was shown -- an accent
    /// held for, words dictated: bytes `range` of it, as it was at
    /// `revision`, are `text` now. One for a field without the keyboard
    /// since, or text since changed, is not taken. False when the edit is
    /// no field's.
    pub(crate) fn plugin_field_replace(
        &mut self,
        token: u64,
        revision: u64,
        range: std::ops::Range<usize>,
        text: &str,
    ) -> bool {
        let Some((view, shown_field)) = self
            .plugin_field_snapshot
            .as_ref()
            .filter(|held| held.token == token)
            .map(|held| (held.view, held.field.clone()))
        else {
            return false;
        };
        let Some(panel) = self.right_sidebar_plugin.as_mut() else {
            return true;
        };
        let shown = if panel.shown.view == view {
            &mut panel.shown
        } else {
            match panel
                .extended
                .as_mut()
                .filter(|(extended, _)| extended.view == view)
            {
                Some((extended, _)) => extended,
                None => return true,
            }
        };
        let Some(id) = shown
            .player
            .focused_field()
            .filter(|id| *id == shown_field)
            .map(str::to_string)
        else {
            return true;
        };
        let Some(field) = shown.player.field(&id) else {
            return true;
        };
        if field.kind == FieldKind::Secret {
            return true;
        }
        let (limit, lines) = (field_limit(field), field.kind == FieldKind::Lines);
        let Some(editor) = shown.editors.get_mut(&id) else {
            return true;
        };
        let now = editor.input.text();
        if text_revision(now) != revision
            || range.start > range.end
            || !now.is_char_boundary(range.start)
            || !now.is_char_boundary(range.end.min(now.len()))
            || range.end > now.len()
        {
            return true;
        }
        let from = now[..range.start].chars().count();
        let to = from + now[range].chars().count();
        editor.input.caret_set(from, false);
        editor.input.caret_set(to, true);
        insert(&mut editor.input, text, limit, lines);
        editor.follow = true;
        editor.goal = None;
        shown.player.edit_field(&id, editor.input.text());
        shown.edits += 1;
        shown.tell();
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
        true
    }

    /// A press in a field gives it the keyboard and puts the caret there --
    /// a second selects the word, a third the line -- and a drag from it
    /// selects. True when the event was a field's.
    pub(super) fn plugin_field_mouse(
        &mut self,
        event: &MouseEvent,
        context: &dyn WindowOps,
        extended: bool,
    ) -> bool {
        let Some(mut panel) = self.right_sidebar_plugin.take() else {
            return false;
        };
        let streak = match &self.last_mouse_click {
            Some(click) => click.streak,
            None => 1,
        };
        let taken = plugin_field_mouse_in(&mut panel, event, extended, streak);
        self.right_sidebar_plugin = Some(panel);
        if taken {
            context.set_cursor(Some(MouseCursor::Text));
            context.invalidate();
        }
        taken
    }

    /// The wheel over a field of lines with more than it shows scrolls its
    /// lines: whether they moved. `None` when it is not over one.
    pub(super) fn plugin_field_wheel(&mut self, event: &MouseEvent, dy: f32) -> Option<bool> {
        let panel = self.right_sidebar_plugin.as_mut()?;
        let (px, py) = (event.coords.x as f32, event.coords.y as f32);
        let over_extended = panel
            .extended
            .as_ref()
            .is_some_and(|(extended, _)| extended.under(px, py).is_some());
        let shown = panel.view_mut(over_extended)?;
        let (x, y) = shown.under(px, py)?;
        let field = shown.player.field_at(x, y)?;
        if field.kind != FieldKind::Lines {
            return None;
        }
        let id = field.id.clone();
        let editor = shown.editors.get_mut(&id)?;
        let layout = editor.layout.as_ref()?;
        let content = layout.lines.len() as f32 * layout.line_height;
        let room = (content - layout.inner.height()).max(0.0);
        if room <= 0.0 {
            return None;
        }
        let before = editor.scroll;
        editor.scroll = (before + dy).clamp(0.0, room);
        editor.follow = false;
        let moved = editor.scroll != before;
        if moved {
            shown.edits += 1;
        }
        Some(moved)
    }

    /// Lays out what each field of `shown` holds, where it is: one line
    /// scrolled across, or lines wrapped at the box's width and scrolled
    /// down, so the caret of the one with the keyboard shows.
    pub(super) fn lay_out_plugin_fields(
        &self,
        shown: &mut Shown,
        fonts: &PanelFonts,
    ) -> anyhow::Result<()> {
        shown.sync_editors();
        let focused = shown.player.focused_field().map(str::to_string);
        let ids: Vec<String> = shown
            .player
            .fields()
            .map(|field| field.id.clone())
            .collect();
        for id in ids {
            let Some(field) = shown.player.field(&id) else {
                continue;
            };
            let inner = shown.inner(field);
            let focused = focused.as_deref() == Some(id.as_str());
            let kind = field.kind;
            let font = font_of(fonts, field.font);
            let Some(editor) = shown.editors.get_mut(&id) else {
                continue;
            };
            let layout = self.lay_out_field(kind, editor, font, inner, focused)?;
            editor.layout = Some(layout);
        }
        Ok(())
    }

    fn lay_out_field(
        &self,
        kind: FieldKind,
        editor: &mut Editor,
        (font, metrics): &(Rc<LoadedFont>, RenderMetrics),
        inner: RectF,
        focused: bool,
    ) -> anyhow::Result<FieldLayout> {
        let line_height = metrics.cell_size.height as f32;
        let caret_width = self.ui_f32(CARET_WIDTH).max(1.0);
        let text = shown_text(kind, editor.input.text());
        let wrap = (kind == FieldKind::Lines).then(|| (inner.width() - caret_width).max(1.0));
        let mut lines = Vec::new();
        let mut starts = Vec::new();
        let mut base = 0;
        for (of, line) in text.split('\n').enumerate() {
            starts.push(base);
            let xs = self.boundaries(font, metrics, line)?;
            let count = xs.len() - 1;
            match wrap {
                Some(width) => wrap_line(&mut lines, line, &xs, base, of, width),
                None => lines.push(LaidLine {
                    start: base,
                    end: base + count,
                    of,
                    xs,
                }),
            }
            base += count + 1;
        }
        let (row, caret_x) = caret_at(&lines, editor.input.cursor);
        let follow = std::mem::take(&mut editor.follow) && focused;
        let (left, top) = if kind == FieldKind::Lines {
            let content = lines.len() as f32 * line_height;
            if follow {
                let caret_top = row as f32 * line_height;
                if caret_top < editor.scroll {
                    editor.scroll = caret_top;
                }
                if caret_top + line_height > editor.scroll + inner.height() {
                    editor.scroll = caret_top + line_height - inner.height();
                }
            }
            editor.scroll = editor
                .scroll
                .clamp(0.0, (content - inner.height()).max(0.0));
            (inner.min_x(), inner.min_y() - editor.scroll)
        } else {
            let wide = lines.first().and_then(|line| line.xs.last()).copied();
            let room = (inner.width() - caret_width).max(0.0);
            if !focused {
                editor.scroll = 0.0;
            } else if follow {
                if caret_x - editor.scroll > room {
                    editor.scroll = caret_x - room;
                }
                if caret_x < editor.scroll {
                    editor.scroll = caret_x;
                }
            }
            editor.scroll = editor
                .scroll
                .clamp(0.0, (wide.unwrap_or(0.0) - room).max(0.0));
            let top = inner.min_y() + ((inner.height() - line_height) / 2.0).round();
            (inner.min_x() - editor.scroll, top)
        };
        Ok(FieldLayout {
            left,
            top,
            line_height,
            inner,
            lines,
            starts,
        })
    }

    /// Where each boundary between the characters of `text` is when it is
    /// set on one line, from its left: as many as it has characters, and
    /// one. The characters inside a cluster -- a ligature's -- share the
    /// cluster's start.
    fn boundaries(
        &self,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        text: &str,
    ) -> anyhow::Result<Vec<f32>> {
        let starts: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
        let count = starts.len();
        let mut xs: Vec<Option<f32>> = vec![None; count + 1];
        xs[0] = Some(0.0);
        if count > 0 {
            let (shaped, _) = self.cached_ui_shape(font, metrics, text)?;
            let mut x = 0.0;
            for info in shaped.iter() {
                let at = starts.partition_point(|start| *start < info.cluster);
                if let Some(slot) = xs.get_mut(at).filter(|slot| slot.is_none()) {
                    *slot = Some(x);
                }
                x += info.glyph.x_advance.get() as f32;
            }
            xs[count] = Some(x);
        }
        let mut last = 0.0;
        Ok(xs
            .into_iter()
            .map(|x| {
                last = x.unwrap_or(last).max(last);
                last
            })
            .collect())
    }

    /// What field `field` of `shown` holds -- or, empty and without the
    /// keyboard, its placeholder -- and what is selected, as laid out:
    /// recorded with the panel, cut to inside the box.
    pub(super) fn paint_plugin_field_text(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        shown: &Shown,
        field: &Field,
        fonts: &PanelFonts,
        chrome: &UiPalette,
        budget: &mut usize,
    ) -> anyhow::Result<()> {
        let Some(editor) = shown.editors.get(&field.id) else {
            return Ok(());
        };
        let Some(layout) = editor.layout.as_ref() else {
            return Ok(());
        };
        let (font, metrics) = font_of(fonts, field.font);
        let inner = layout.inner;
        let focused = shown.player.focused_field() == Some(field.id.as_str());
        let text = editor.input.text();
        if text.is_empty() {
            if focused || field.placeholder.is_empty() {
                return Ok(());
            }
            let placeholder = match field.font {
                Font::Ui => {
                    self.ellipsize_ui_text(font, &field.placeholder, inner.width() as usize)?
                }
                Font::Mono => Cow::Borrowed(field.placeholder.as_str()),
            };
            let (shaped, _) = self.cached_ui_shape(font, metrics, &placeholder)?;
            if !spend(budget, shaped.len()) {
                return Ok(());
            }
            let faint = chrome.secondary_text.mul_alpha(0.72);
            self.paint_cached_ui_shape_pixel_clipped(
                layers,
                metrics,
                &shaped,
                inner.min_x(),
                layout.top,
                inner.min_x(),
                inner.max_x(),
                |_| faint,
            )?;
            return Ok(());
        }
        let shown_text = shown_text(field.kind, text);
        let pieces: Vec<&str> = shown_text.split('\n').collect();
        let selection = editor.input.caret_selection_range();
        let selected = chrome.selected_bg.mul_alpha(0.55);
        for (row, line) in layout.lines.iter().enumerate() {
            let top = layout.top + row as f32 * layout.line_height;
            if top + layout.line_height <= inner.min_y() || top >= inner.max_y() {
                continue;
            }
            if let Some((from, to)) = selection {
                let (a, b) = (from.max(line.start), to.min(line.end));
                // A selection going on past a line's end takes a little of
                // the line's end with it: the line break it selects.
                let on = layout
                    .lines
                    .get(row + 1)
                    .is_some_and(|next| next.start > line.end)
                    && to > line.end
                    && from <= line.end;
                if a < b || on {
                    let left = layout.left + line.xs[a.min(line.end) - line.start];
                    let mut right = layout.left + line.xs[b.max(a).min(line.end) - line.start];
                    if on {
                        right += layout.line_height / 3.0;
                    }
                    let left = left.max(inner.min_x());
                    let right = right.min(inner.max_x());
                    if right > left && spend(budget, 1) {
                        let area: RectF = euclid::rect(left, top, right - left, layout.line_height);
                        self.filled_rectangle(layers, LAYER, area, selected)?;
                    }
                }
            }
            if line.end == line.start {
                continue;
            }
            let piece = pieces.get(line.of).copied().unwrap_or_default();
            let (shaped, _) = self.cached_ui_shape(font, metrics, piece)?;
            let starts: Vec<usize> = piece.char_indices().map(|(at, _)| at).collect();
            let char_of = |cluster: usize| starts.partition_point(|start| *start < cluster);
            let base = layout.starts.get(line.of).copied().unwrap_or(0);
            let (from, to) = (line.start - base, line.end - base);
            let first = shaped.partition_point(|info| char_of(info.cluster) < from);
            let last = shaped.partition_point(|info| char_of(info.cluster) < to);
            let glyphs = &shaped[first..last.max(first)];
            if !spend(budget, glyphs.len()) {
                return Ok(());
            }
            self.paint_cached_ui_shape_pixel_clipped(
                layers,
                metrics,
                glyphs,
                layout.left,
                top,
                inner.min_x(),
                inner.max_x(),
                |_| chrome.text,
            )?;
        }
        Ok(())
    }

    /// The outline of a view with the keyboard in none of its fields --
    /// given it by the user's key -- over whatever it shows.
    pub(super) fn paint_plugin_view_ring(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        shown: &Shown,
        chrome: &UiPalette,
    ) -> anyhow::Result<()> {
        if shown.player.keyboard() != &Keyboard::Panel {
            return Ok(());
        }
        let env = shown.player.env();
        let area: RectF = euclid::rect(
            shown.origin.0,
            shown.origin.1,
            env.width * shown.scale,
            env.height * shown.scale,
        );
        let ring = self.ui_f32(RING_WIDTH).max(1.0);
        self.stroke_panel_rect(layers, area, chrome.accent, 0.0, ring)
    }

    /// Over what was recorded, each time it is painted: the caret of the
    /// field with the keyboard, blinking, or the text being composed there,
    /// with the input method's candidates put by it.
    pub(super) fn paint_plugin_field_caret(
        &self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
        shown: &Shown,
        fonts: &PanelFonts,
        chrome: &UiPalette,
    ) -> anyhow::Result<()> {
        let Some(id) = shown.player.focused_field() else {
            return Ok(());
        };
        let (Some(field), Some(editor)) = (shown.player.field(id), shown.editors.get(id)) else {
            return Ok(());
        };
        let Some(layout) = editor.layout.as_ref() else {
            return Ok(());
        };
        let (font, metrics) = font_of(fonts, field.font);
        let inner = layout.inner;
        let caret_width = self.ui_f32(CARET_WIDTH).max(1.0);
        let (row, x) = caret_at(&layout.lines, editor.input.cursor);
        let left = (layout.left + x).clamp(
            inner.min_x(),
            (inner.max_x() - caret_width).max(inner.min_x()),
        );
        let top = layout.top + row as f32 * layout.line_height;
        if top + layout.line_height <= inner.min_y() || top >= inner.max_y() {
            return Ok(());
        }
        if let Some(window) = self.window.as_ref() {
            window.set_text_cursor_position(Rect::new(
                Point::new(left as isize, top as isize),
                metrics.cell_size,
            ));
        }
        if let DeadKeyStatus::Composing(composing) = &self.dead_key_status {
            // A secret field's shows as dots too, being composed.
            let composing = shown_text(field.kind, composing);
            let (shaped, _) = self.cached_ui_shape(font, metrics, &composing)?;
            let wide = self
                .paint_cached_ui_shape_pixel_clipped(
                    layers,
                    metrics,
                    &shaped,
                    left,
                    top,
                    left,
                    inner.max_x(),
                    |_| chrome.text,
                )?
                .min(inner.max_x() - left);
            if wide > 0.0 {
                let under: RectF = euclid::rect(left, top + layout.line_height - 1.0, wide, 1.0);
                self.filled_rectangle(layers, LAYER, under, chrome.text)?;
            }
            return Ok(());
        }
        if editor.input.caret_selection_range().is_some() || !self.right_sidebar_snippet_cursor_on()
        {
            return Ok(());
        }
        let visible_top = top.max(inner.min_y());
        let visible_bottom = (top + layout.line_height).min(inner.max_y());
        let caret: RectF =
            euclid::rect(left, visible_top, caret_width, visible_bottom - visible_top);
        self.filled_rectangle(layers, LAYER, caret, chrome.text)?;
        Ok(())
    }
}

/// [`TermWindow::plugin_field_mouse`], with the panel in hand.
fn plugin_field_mouse_in(
    panel: &mut PluginPanel,
    event: &MouseEvent,
    extended: bool,
    streak: usize,
) -> bool {
    let (px, py) = (event.coords.x as f32, event.coords.y as f32);
    match event.kind {
        WMEK::Press(MousePress::Left) => {
            let Some(shown) = panel.view_mut(extended) else {
                return false;
            };
            // What a program that stopped, or is starting again, last drew
            // takes no typing: there is nobody to hear it.
            if !matches!(shown.status, Status::Open) {
                return false;
            }
            let (x, y) = shown.units(px, py);
            let Some(id) = shown.player.field_at(x, y).map(|field| field.id.clone()) else {
                return false;
            };
            let clearing = shown.clear_under(px, py).is_some();
            // The keyboard comes to this view: the other lets go of it.
            if let Some(other) = panel.view_mut(!extended) {
                if other.player.has_keyboard() {
                    other.player.blur();
                    other.tell();
                }
            }
            let Some(shown) = panel.view_mut(extended) else {
                return false;
            };
            shown.sync_editors();
            let again = shown.player.focused_field() == Some(id.as_str());
            shown.player.focus_field(&id);
            // Its button empties it, and leaves the keyboard in it.
            if clearing {
                if let Some(editor) = shown.editors.get_mut(&id) {
                    editor.input.clear();
                    editor.goal = None;
                    editor.follow = true;
                }
                shown.player.edit_field(&id, "");
                shown.edits += 1;
                shown.tell();
                return true;
            }
            let lines = shown
                .player
                .field(&id)
                .is_some_and(|field| field.kind == FieldKind::Lines);
            if let Some(editor) = shown.editors.get_mut(&id) {
                let at = editor
                    .layout
                    .as_ref()
                    .map_or(editor.input.char_len(), |layout| index_at(layout, px, py));
                let extend = again && event.modifiers.contains(window::Modifiers::SHIFT);
                match streak {
                    0 | 1 => editor.input.caret_set(at, extend),
                    2 => {
                        editor.input.caret_set(at, false);
                        editor.input.caret_select_word();
                    }
                    _ => select_line(&mut editor.input, at, lines),
                }
                editor.goal = None;
                editor.follow = true;
            }
            shown.edits += 1;
            shown.tell();
            panel.selecting = Some((extended, id));
            true
        }
        WMEK::Move if event.mouse_buttons.contains(MouseButtons::LEFT) => {
            let Some((from, id)) = panel.selecting.clone() else {
                return false;
            };
            if from != extended {
                return false;
            }
            let Some(shown) = panel.view_mut(extended) else {
                return false;
            };
            let Some(editor) = shown.editors.get_mut(&id) else {
                return false;
            };
            let Some(layout) = editor.layout.as_ref() else {
                return true;
            };
            let at = index_at(layout, px, py);
            if at != editor.input.cursor || editor.input.selection_anchor.is_none() {
                editor.input.caret_set(at, true);
                editor.follow = true;
                shown.edits += 1;
            }
            true
        }
        WMEK::Release(MousePress::Left) => {
            panel.selecting = None;
            false
        }
        _ => false,
    }
}

/// The fonts a field is set in: the sidebar's, at the body size, or the
/// terminal's.
fn font_of(fonts: &PanelFonts, font: Font) -> &(Rc<LoadedFont>, RenderMetrics) {
    match font {
        Font::Ui => &fonts.body,
        Font::Mono => &fonts.mono,
    }
}

/// What a field of `kind` shows of `text`: a secret one, a dot for each
/// character.
fn shown_text(kind: FieldKind, text: &str) -> Cow<'_, str> {
    match kind {
        FieldKind::Secret => Cow::Owned("\u{2022}".repeat(text.chars().count())),
        FieldKind::Line | FieldKind::Lines => Cow::Borrowed(text),
    }
}

/// What a field's text is at, for the system's edits of it to be taken
/// only of the text they were made for.
fn text_revision(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// What a copy takes: what is selected, or all of it.
fn copied(input: &TextInputState) -> String {
    input
        .caret_selected_text()
        .unwrap_or_else(|| input.text().to_string())
}

/// Puts `text` in at the caret, over what is selected: no more than the
/// field takes, and on one line unless it has `lines`.
fn insert(input: &mut TextInputState, text: &str, limit: usize, lines: bool) {
    let selected = input
        .caret_selection_range()
        .map_or(0, |(from, to)| to - from);
    let room = limit.saturating_sub(input.char_len().saturating_sub(selected));
    let text: String = text
        .chars()
        .filter(|ch| match ch {
            '\n' | '\t' => lines,
            ch => !ch.is_control(),
        })
        .take(room)
        .collect();
    input.caret_insert(&text, lines);
}

/// Lays `text`, one of a field's lines, whose boundaries are at `xs`, as
/// lines no wider than `width`: broken at the last place that keeps one
/// within it -- after a space, or between characters of a script written
/// without spaces -- or else where it overflows.
fn wrap_line(
    lines: &mut Vec<LaidLine>,
    text: &str,
    xs: &[f32],
    base: usize,
    of: usize,
    width: f32,
) {
    let chars: Vec<char> = text.chars().collect();
    let count = chars.len();
    if count == 0 {
        lines.push(LaidLine {
            start: base,
            end: base,
            of,
            xs: vec![0.0],
        });
        return;
    }
    let mut start = 0;
    while start < count {
        let mut end = start + 1;
        while end < count && xs[end + 1] - xs[start] <= width {
            end += 1;
        }
        if end < count {
            if let Some(at) = (start + 1..=end)
                .rev()
                .find(|at| breaks_before(&chars, *at))
            {
                end = at;
            }
        }
        let origin = xs[start];
        lines.push(LaidLine {
            start: base + start,
            end: base + end,
            of,
            xs: xs[start..=end].iter().map(|x| x - origin).collect(),
        });
        start = end;
    }
}

/// Whether a line may break before character `at` of `chars`: after a
/// space, or where the characters on either side are of a script written
/// without spaces -- but not before the punctuation that closes one.
fn breaks_before(chars: &[char], at: usize) -> bool {
    let (Some(before), Some(after)) = (chars.get(at.wrapping_sub(1)), chars.get(at)) else {
        return false;
    };
    if before.is_whitespace() {
        return true;
    }
    let closing = matches!(
        after,
        '、' | '。'
            | '，'
            | '．'
            | '：'
            | '；'
            | '！'
            | '？'
            | '）'
            | '」'
            | '』'
            | '】'
            | '〉'
            | '》'
            | '〕'
            | 'ー'
            | '…'
    );
    (unspaced(*before) || unspaced(*after)) && !after.is_whitespace() && !closing
}

/// A character of a script written without spaces between its words:
/// Chinese, Japanese, Korean, and their punctuation.
fn unspaced(ch: char) -> bool {
    matches!(
        ch as u32,
        0x2E80..=0x2FFF
            | 0x3000..=0x303F
            | 0x3040..=0x30FF
            | 0x3100..=0x31FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xAC00..=0xD7AF
            | 0xF900..=0xFAFF
            | 0xFF00..=0xFFEF
            | 0x20000..=0x3FFFF
    )
}

/// The laid line the caret before character `index` shows on, and where
/// across it: at a line wrapped onto the next, the caret at the break is
/// the next line's.
fn caret_at(lines: &[LaidLine], index: usize) -> (usize, f32) {
    for (row, line) in lines.iter().enumerate() {
        let wrapped = lines
            .get(row + 1)
            .is_some_and(|next| next.start == line.end);
        if index >= line.start && (index < line.end || (index == line.end && !wrapped)) {
            return (row, line.xs[index - line.start]);
        }
    }
    let last = lines.len().saturating_sub(1);
    let x = lines
        .last()
        .and_then(|line| line.xs.last())
        .copied()
        .unwrap_or(0.0);
    (last, x)
}

/// The character the pointer at `x`, `y` is nearest the start of.
fn index_at(layout: &FieldLayout, x: f32, y: f32) -> usize {
    let rows = layout.lines.len();
    if rows == 0 {
        return 0;
    }
    let row = (((y - layout.top) / layout.line_height).floor().max(0.0) as usize).min(rows - 1);
    nearest(&layout.lines, row, x - layout.left)
}

/// The boundary of laid line `row` nearest `x` across it: short of the
/// break of a line wrapped onto the next, where the caret would show on that.
fn nearest(lines: &[LaidLine], row: usize, x: f32) -> usize {
    let line = &lines[row];
    let at = line
        .xs
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| (*a - x).abs().total_cmp(&(*b - x).abs()))
        .map_or(0, |(at, _)| at);
    let index = line.start + at;
    let wrapped = lines
        .get(row + 1)
        .is_some_and(|next| next.start == line.end);
    if wrapped && index == line.end && line.end > line.start {
        index - 1
    } else {
        index
    }
}

/// Moves the caret `rows` laid lines down -- up, for fewer than none --
/// keeping to the x it went up and down from.
fn vertical(editor: &mut Editor, rows: isize, shift: bool) {
    let Some(layout) = editor.layout.as_ref() else {
        return;
    };
    let (row, x) = caret_at(&layout.lines, editor.input.cursor);
    let goal = editor.goal.unwrap_or(x);
    let target = row as isize + rows;
    let at = if target < 0 {
        0
    } else if target as usize >= layout.lines.len() {
        editor.input.char_len()
    } else {
        nearest(&layout.lines, target as usize, goal)
    };
    editor.input.caret_set(at, shift);
    editor.goal = Some(goal);
}

/// How many laid lines a page up or down moves: what shows, less one.
fn page_rows(editor: &Editor) -> isize {
    editor.layout.as_ref().map_or(1, |layout| {
        ((layout.inner.height() / layout.line_height).floor() as isize - 1).max(1)
    })
}

/// The laid line the caret is on, as where it starts and where a caret at
/// its end goes.
fn laid_line(editor: &Editor) -> Option<(usize, usize)> {
    let layout = editor.layout.as_ref()?;
    let (row, _) = caret_at(&layout.lines, editor.input.cursor);
    let line = layout.lines.get(row)?;
    let wrapped = layout
        .lines
        .get(row + 1)
        .is_some_and(|next| next.start == line.end);
    let end = if wrapped && line.end > line.start {
        line.end - 1
    } else {
        line.end
    };
    Some((line.start, end))
}

fn line_home(editor: &mut Editor, shift: bool) {
    match laid_line(editor) {
        Some((start, _)) => editor.input.caret_set(start, shift),
        None => editor.input.caret_move_home(shift),
    }
}

fn line_end(editor: &mut Editor, shift: bool) {
    match laid_line(editor) {
        Some((_, end)) => editor.input.caret_set(end, shift),
        None => editor.input.caret_move_end(shift),
    }
}

/// Deletes from the start of the laid line the caret is on to the caret --
/// or what is selected.
fn delete_to_line_start(editor: &mut Editor) {
    if editor.input.caret_delete_selection() {
        return;
    }
    let start = laid_line(editor).map_or(0, |(start, _)| start);
    editor.input.clear_selection();
    editor.input.caret_set(start, true);
    editor.input.caret_delete_selection();
}

/// Selects the line character `at` is on, in a field of lines; all of a
/// field of one.
fn select_line(input: &mut TextInputState, at: usize, lines: bool) {
    if !lines {
        input.caret_select_all();
        return;
    }
    let chars: Vec<char> = input.text().chars().collect();
    let at = at.min(chars.len());
    let start = chars[..at]
        .iter()
        .rposition(|ch| *ch == '\n')
        .map_or(0, |newline| newline + 1);
    let end = chars[at..]
        .iter()
        .position(|ch| *ch == '\n')
        .map_or(chars.len(), |newline| at + newline);
    input.caret_set(start, false);
    input.caret_set(end, true);
}

/// The modifiers of a key, as the player is told them.
fn player_mods(mods: TermModifiers) -> Mods {
    Mods {
        shift: mods.contains(TermModifiers::SHIFT),
        ctrl: mods.contains(TermModifiers::CTRL),
        alt: mods.contains(TermModifiers::ALT),
        cmd: mods.contains(TermModifiers::SUPER),
    }
}

/// A key as a browser names it (`KeyboardEvent.key`), which is how a panel
/// says the keys it takes; `None` for one it has no name for.
fn key_name(key: &TermKeyCode) -> Option<String> {
    let name = match key {
        TermKeyCode::Char(ch) if !ch.is_control() => return Some(ch.to_string()),
        TermKeyCode::Enter => "Enter",
        TermKeyCode::Escape => "Escape",
        TermKeyCode::Tab => "Tab",
        TermKeyCode::Backspace => "Backspace",
        TermKeyCode::Delete => "Delete",
        TermKeyCode::Insert => "Insert",
        TermKeyCode::LeftArrow => "ArrowLeft",
        TermKeyCode::RightArrow => "ArrowRight",
        TermKeyCode::UpArrow => "ArrowUp",
        TermKeyCode::DownArrow => "ArrowDown",
        TermKeyCode::Home => "Home",
        TermKeyCode::End => "End",
        TermKeyCode::PageUp => "PageUp",
        TermKeyCode::PageDown => "PageDown",
        TermKeyCode::Function(n) => return Some(format!("F{n}")),
        _ => return None,
    };
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines laid as if every character were 10 wide.
    fn laid(text: &str, width: Option<f32>) -> Vec<LaidLine> {
        let mut lines = Vec::new();
        let mut base = 0;
        for (of, piece) in text.split('\n').enumerate() {
            let count = piece.chars().count();
            let xs: Vec<f32> = (0..=count).map(|n| n as f32 * 10.0).collect();
            match width {
                Some(width) => wrap_line(&mut lines, piece, &xs, base, of, width),
                None => lines.push(LaidLine {
                    start: base,
                    end: base + count,
                    of,
                    xs,
                }),
            }
            base += count + 1;
        }
        lines
    }

    fn spans(lines: &[LaidLine]) -> Vec<(usize, usize)> {
        lines.iter().map(|line| (line.start, line.end)).collect()
    }

    #[test]
    fn lines_wrap_after_a_space_or_where_they_overflow() {
        // "aaa bbb" at 50 wide: "aaa " then "bbb".
        assert_eq!(spans(&laid("aaa bbb", Some(50.0))), [(0, 4), (4, 7)]);
        // No space to break at: cut where it overflows.
        assert_eq!(
            spans(&laid("abcdefgh", Some(30.0))),
            [(0, 3), (3, 6), (6, 8)]
        );
        // A line break starts a line; an empty line is a line.
        assert_eq!(
            spans(&laid("ab\n\ncd", Some(50.0))),
            [(0, 2), (3, 3), (4, 6)]
        );
        // Never fewer than a character a line, however narrow.
        assert_eq!(spans(&laid("ab", Some(1.0))), [(0, 1), (1, 2)]);
        // Between Chinese characters, wherever it fills -- but never before
        // the punctuation that closes a phrase.
        assert_eq!(spans(&laid("ab 中文字", Some(50.0))), [(0, 5), (5, 6)]);
        assert_eq!(spans(&laid("中文字。好", Some(30.0))), [(0, 2), (2, 5)]);
    }

    #[test]
    fn the_caret_at_a_wrap_is_on_the_next_line_and_a_press_past_the_end_stays() {
        let lines = laid("aaa bbb", Some(50.0));
        assert_eq!(
            caret_at(&lines, 4),
            (1, 0.0),
            "the break is the next line's"
        );
        assert_eq!(caret_at(&lines, 3), (0, 30.0));
        assert_eq!(caret_at(&lines, 7), (1, 30.0));
        assert_eq!(nearest(&lines, 0, 100.0), 3, "short of the break");
        assert_eq!(nearest(&lines, 1, 100.0), 7);
        // At a line break the caret stays on its line.
        let lines = laid("ab\ncd", Some(50.0));
        assert_eq!(caret_at(&lines, 2), (0, 20.0));
        assert_eq!(caret_at(&lines, 3), (1, 0.0));
    }

    #[test]
    fn a_caret_goes_up_and_down_keeping_to_its_x() {
        let mut editor = Editor::new("abcdef\nab\nabcdef", 0);
        let lines = laid("abcdef\nab\nabcdef", None);
        editor.layout = Some(FieldLayout {
            left: 0.0,
            top: 0.0,
            line_height: 10.0,
            inner: euclid::rect(0.0, 0.0, 100.0, 30.0),
            lines,
            starts: vec![0, 7, 10],
        });
        editor.input.caret_set(5, false);
        vertical(&mut editor, 1, false);
        assert_eq!(editor.input.cursor, 9, "the short line's end");
        vertical(&mut editor, 1, false);
        assert_eq!(editor.input.cursor, 15, "back to where it was across");
        vertical(&mut editor, 1, false);
        assert_eq!(editor.input.cursor, 16, "below the last, the end");
        vertical(&mut editor, -9, true);
        assert_eq!(editor.input.cursor, 0);
        assert_eq!(editor.input.caret_selection_range(), Some((0, 16)));
    }

    #[test]
    fn what_is_put_in_is_kept_to_what_the_field_takes() {
        let mut input = TextInputState::new();
        insert(&mut input, "ab\ncd\u{7}", 3, false);
        assert_eq!(
            input.text(),
            "abc",
            "one line, no control characters, three at most"
        );
        input.caret_select_all();
        insert(&mut input, "xyzw", 3, false);
        assert_eq!(input.text(), "xyz", "over the selection");
        let mut lines = TextInputState::new();
        insert(&mut lines, "a\n\tb", 10, true);
        assert_eq!(lines.text(), "a\n\tb");
    }

    #[test]
    fn a_line_is_selected_within_its_breaks() {
        let mut input = TextInputState::new();
        input.set_text_end("one\ntwo\nthree".into());
        select_line(&mut input, 5, true);
        assert_eq!(input.caret_selected_text().as_deref(), Some("two"));
        select_line(&mut input, 5, false);
        assert_eq!(
            input.caret_selected_text().as_deref(),
            Some("one\ntwo\nthree")
        );
    }

    #[test]
    fn keys_are_named_as_a_browser_names_them() {
        assert_eq!(
            key_name(&TermKeyCode::DownArrow).as_deref(),
            Some("ArrowDown")
        );
        assert_eq!(key_name(&TermKeyCode::Char('j')).as_deref(), Some("j"));
        assert_eq!(key_name(&TermKeyCode::Char(' ')).as_deref(), Some(" "));
        assert_eq!(key_name(&TermKeyCode::Function(5)).as_deref(), Some("F5"));
        assert_eq!(key_name(&TermKeyCode::CapsLock), None);
        assert_eq!(
            shown_text(FieldKind::Secret, "ab中"),
            "\u{2022}\u{2022}\u{2022}"
        );
    }
}
