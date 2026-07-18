use parking_lot::Mutex;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Affinity {
    Before,
    After,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SourcePosition {
    pub byte: usize,
    pub affinity: Affinity,
}

impl SourcePosition {
    pub(crate) fn new(byte: usize) -> Self {
        Self {
            byte,
            affinity: Affinity::After,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SourceSelection {
    pub anchor: SourcePosition,
    pub focus: SourcePosition,
}

impl SourceSelection {
    pub(crate) fn caret(byte: usize) -> Self {
        let position = SourcePosition::new(byte);
        Self {
            anchor: position,
            focus: position,
        }
    }

    pub(crate) fn range(self) -> Range<usize> {
        self.anchor.byte.min(self.focus.byte)..self.anchor.byte.max(self.focus.byte)
    }

    pub(crate) fn is_caret(self) -> bool {
        self.anchor.byte == self.focus.byte
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditorMode {
    LivePreview,
    Source,
    ReadOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SaveState {
    Saved,
    Dirty,
    Saving(u64),
    Failed(String),
}

#[derive(Debug, Clone)]
pub(crate) struct EditorViewState {
    pub selection: SourceSelection,
    pub preferred_column: Option<usize>,
    pub scroll_offset: f32,
    pub mode: EditorMode,
    pub focused: bool,
    pub preedit: Option<String>,
}

impl Default for EditorViewState {
    fn default() -> Self {
        Self {
            selection: SourceSelection::caret(0),
            preferred_column: None,
            scroll_offset: 0.0,
            mode: EditorMode::LivePreview,
            focused: false,
            preedit: None,
        }
    }
}

#[derive(Debug, Clone)]
struct EditRecord {
    start: usize,
    deleted: String,
    inserted: String,
    before: SourceSelection,
    after: SourceSelection,
}

#[derive(Debug)]
pub(crate) struct MarkdownDocumentSession {
    #[allow(dead_code)]
    pub document_id: String,
    pub path: PathBuf,
    source: String,
    revision: u64,
    saved_revision: u64,
    save_state: SaveState,
    line_starts: Vec<usize>,
    undo: Vec<EditRecord>,
    redo: Vec<EditRecord>,
    save_lock: Arc<Mutex<()>>,
}

const MAX_UNDO_RECORDS: usize = 512;

impl MarkdownDocumentSession {
    pub(crate) fn new(document_id: String, path: PathBuf, source: String) -> Self {
        let line_starts = line_starts(&source);
        Self {
            document_id,
            path,
            source,
            revision: 0,
            saved_revision: 0,
            save_state: SaveState::Saved,
            line_starts,
            undo: vec![],
            redo: vec![],
            save_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn save_state(&self) -> &SaveState {
        &self.save_state
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    pub(crate) fn snapshot_for_save(&mut self) -> (u64, PathBuf, String) {
        let revision = self.revision;
        self.save_state = SaveState::Saving(revision);
        (revision, self.path.clone(), self.source.clone())
    }

    pub(crate) fn save_lock(&self) -> Arc<Mutex<()>> {
        Arc::clone(&self.save_lock)
    }

    pub(crate) fn finish_save(&mut self, revision: u64, result: anyhow::Result<()>) {
        match result {
            Ok(()) => {
                self.saved_revision = self.saved_revision.max(revision);
                self.save_state = if self.saved_revision == self.revision {
                    SaveState::Saved
                } else {
                    SaveState::Dirty
                };
            }
            Err(err) => self.save_state = SaveState::Failed(format!("{err:#}")),
        }
    }

    pub(crate) fn selected_text(&self, view: &EditorViewState) -> Option<&str> {
        let range = self.clamped_selection_range(view);
        (range.start != range.end).then(|| &self.source[range])
    }

    pub(crate) fn set_caret(&self, view: &mut EditorViewState, byte: usize, extend: bool) {
        let byte = clamp_char_boundary(&self.source, byte);
        if extend {
            view.selection.focus = SourcePosition::new(byte);
        } else {
            view.selection = SourceSelection::caret(byte);
        }
        view.preferred_column = None;
    }

    pub(crate) fn select_all(&self, view: &mut EditorViewState) {
        view.selection = SourceSelection {
            anchor: SourcePosition::new(0),
            focus: SourcePosition::new(self.source.len()),
        };
        view.preferred_column = None;
    }

    pub(crate) fn clamp_view(&self, view: &mut EditorViewState) {
        view.selection.anchor.byte = clamp_char_boundary(&self.source, view.selection.anchor.byte);
        view.selection.focus.byte = clamp_char_boundary(&self.source, view.selection.focus.byte);
    }

    pub(crate) fn insert_text(&mut self, view: &mut EditorViewState, text: &str) -> bool {
        if view.mode == EditorMode::ReadOnly {
            return false;
        }
        let filtered: String = text
            .chars()
            .filter_map(|ch| match ch {
                '\r' => None,
                '\n' | '\t' => Some(ch),
                _ if ch.is_control() => None,
                _ => Some(ch),
            })
            .collect();
        if filtered.is_empty() {
            return false;
        }
        let range = self.clamped_selection_range(view);
        self.replace_range(view, range, &filtered)
    }

    pub(crate) fn surround_selection(
        &mut self,
        view: &mut EditorViewState,
        prefix: &str,
        suffix: &str,
    ) -> bool {
        if view.mode == EditorMode::ReadOnly {
            return false;
        }
        let range = self.clamped_selection_range(view);
        let selected_len = range.end.saturating_sub(range.start);
        let inserted = format!("{prefix}{}{suffix}", &self.source[range.clone()]);
        let start = range.start;
        if !self.replace_range(view, range, &inserted) {
            return false;
        }
        view.selection = if selected_len == 0 {
            SourceSelection::caret(start + prefix.len())
        } else {
            SourceSelection {
                anchor: SourcePosition::new(start + prefix.len()),
                focus: SourcePosition::new(start + prefix.len() + selected_len),
            }
        };
        if let Some(edit) = self.undo.last_mut() {
            edit.after = view.selection;
        }
        true
    }

    pub(crate) fn insert_newline(&mut self, view: &mut EditorViewState) -> bool {
        if view.mode == EditorMode::ReadOnly {
            return false;
        }
        if !view.selection.is_caret() {
            return self.insert_text(view, "\n");
        }
        let caret = clamp_char_boundary(&self.source, view.selection.focus.byte);
        let (line_start, line_end) = self.line_bounds(caret);
        let before = &self.source[line_start..caret];
        let indent_len = before
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        let indent = &before[..indent_len];
        let content = &before[indent_len..];
        let continuation = markdown_line_continuation(content);
        if continuation.is_some() && markdown_line_is_empty_marker(content) && caret == line_end {
            view.selection = SourceSelection {
                anchor: SourcePosition::new(line_start),
                focus: SourcePosition::new(caret),
            };
            return self.backspace(view);
        }
        let inserted = match continuation {
            Some(marker) => format!("\n{indent}{marker}"),
            None => format!("\n{indent}"),
        };
        self.insert_text(view, &inserted)
    }

    pub(crate) fn backspace(&mut self, view: &mut EditorViewState) -> bool {
        if view.mode == EditorMode::ReadOnly {
            return false;
        }
        let selection = view.selection.range();
        if selection.start != selection.end {
            return self.replace_range(view, selection, "");
        }
        let caret = view.selection.focus.byte;
        let previous = previous_grapheme_boundary(&self.source, caret);
        if previous == caret {
            return false;
        }
        self.replace_range(view, previous..caret, "")
    }

    pub(crate) fn delete_forward(&mut self, view: &mut EditorViewState) -> bool {
        if view.mode == EditorMode::ReadOnly {
            return false;
        }
        let selection = view.selection.range();
        if selection.start != selection.end {
            return self.replace_range(view, selection, "");
        }
        let caret = view.selection.focus.byte;
        let next = next_grapheme_boundary(&self.source, caret);
        if next == caret {
            return false;
        }
        self.replace_range(view, caret..next, "")
    }

    pub(crate) fn move_left(&self, view: &mut EditorViewState, extend: bool) {
        if !extend && !view.selection.is_caret() {
            self.set_caret(view, view.selection.range().start, false);
            return;
        }
        let target = previous_grapheme_boundary(&self.source, view.selection.focus.byte);
        self.set_caret(view, target, extend);
    }

    pub(crate) fn move_right(&self, view: &mut EditorViewState, extend: bool) {
        if !extend && !view.selection.is_caret() {
            self.set_caret(view, view.selection.range().end, false);
            return;
        }
        let target = next_grapheme_boundary(&self.source, view.selection.focus.byte);
        self.set_caret(view, target, extend);
    }

    pub(crate) fn move_word_left(&self, view: &mut EditorViewState, extend: bool) {
        let target = previous_word_boundary(&self.source, view.selection.focus.byte);
        self.set_caret(view, target, extend);
    }

    pub(crate) fn move_word_right(&self, view: &mut EditorViewState, extend: bool) {
        let target = next_word_boundary(&self.source, view.selection.focus.byte);
        self.set_caret(view, target, extend);
    }

    pub(crate) fn delete_word_back(&mut self, view: &mut EditorViewState) -> bool {
        if view.mode == EditorMode::ReadOnly {
            return false;
        }
        let selection = self.clamped_selection_range(view);
        if selection.start != selection.end {
            return self.replace_range(view, selection, "");
        }
        let caret = clamp_char_boundary(&self.source, view.selection.focus.byte);
        let previous = previous_word_boundary(&self.source, caret);
        self.replace_range(view, previous..caret, "")
    }

    pub(crate) fn move_line_start(&self, view: &mut EditorViewState, extend: bool) {
        let (start, _) = self.line_bounds(view.selection.focus.byte);
        self.set_caret(view, start, extend);
    }

    pub(crate) fn move_line_end(&self, view: &mut EditorViewState, extend: bool) {
        let (_, end) = self.line_bounds(view.selection.focus.byte);
        self.set_caret(view, end, extend);
    }

    pub(crate) fn move_vertical(&self, view: &mut EditorViewState, delta: isize, extend: bool) {
        let caret = view.selection.focus.byte;
        let line = self.line_index(caret);
        let (line_start, _) = self.line_bounds(caret);
        let column = grapheme_column(&self.source[line_start..caret]);
        let desired = view.preferred_column.unwrap_or(column);
        let target_line = if delta.is_negative() {
            line.saturating_sub(delta.unsigned_abs())
        } else {
            line.saturating_add(delta as usize)
                .min(self.line_starts.len().saturating_sub(1))
        };
        let start = self.line_starts[target_line];
        let end = self.line_end(target_line);
        let target = byte_for_grapheme_column(&self.source[start..end], desired) + start;
        let anchor = if extend {
            Some(view.selection.anchor)
        } else {
            None
        };
        self.set_caret(view, target, extend);
        if let Some(anchor) = anchor {
            view.selection.anchor = anchor;
        }
        view.preferred_column = Some(desired);
    }

    pub(crate) fn undo(&mut self, view: &mut EditorViewState) -> bool {
        let Some(edit) = self.undo.pop() else {
            return false;
        };
        let end = edit.start + edit.inserted.len();
        self.source.replace_range(edit.start..end, &edit.deleted);
        view.selection = edit.before;
        self.redo.push(edit);
        self.note_mutation();
        true
    }

    pub(crate) fn redo(&mut self, view: &mut EditorViewState) -> bool {
        let Some(edit) = self.redo.pop() else {
            return false;
        };
        let end = edit.start + edit.deleted.len();
        self.source.replace_range(edit.start..end, &edit.inserted);
        view.selection = edit.after;
        self.undo.push(edit);
        self.note_mutation();
        true
    }

    fn replace_range(
        &mut self,
        view: &mut EditorViewState,
        range: Range<usize>,
        inserted: &str,
    ) -> bool {
        let start = clamp_char_boundary(&self.source, range.start);
        let end = clamp_char_boundary(&self.source, range.end).max(start);
        if start == end && inserted.is_empty() {
            return false;
        }
        let before = view.selection;
        let deleted = self.source[start..end].to_string();
        self.source.replace_range(start..end, inserted);
        let after = SourceSelection::caret(start + inserted.len());
        view.selection = after;
        view.preferred_column = None;
        self.undo.push(EditRecord {
            start,
            deleted,
            inserted: inserted.to_string(),
            before,
            after,
        });
        if self.undo.len() > MAX_UNDO_RECORDS {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.note_mutation();
        true
    }

    fn note_mutation(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.save_state = SaveState::Dirty;
        self.line_starts = line_starts(&self.source);
    }

    fn clamped_selection_range(&self, view: &EditorViewState) -> Range<usize> {
        let range = view.selection.range();
        let start = clamp_char_boundary(&self.source, range.start);
        let end = clamp_char_boundary(&self.source, range.end).max(start);
        start..end
    }

    fn line_index(&self, byte: usize) -> usize {
        let byte = byte.min(self.source.len());
        self.line_starts
            .partition_point(|start| *start <= byte)
            .saturating_sub(1)
    }

    fn line_end(&self, line: usize) -> usize {
        self.line_starts
            .get(line + 1)
            .map(|next| next.saturating_sub(1))
            .unwrap_or(self.source.len())
    }

    fn line_bounds(&self, byte: usize) -> (usize, usize) {
        let line = self.line_index(byte);
        (self.line_starts[line], self.line_end(line))
    }
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(
        source
            .match_indices('\n')
            .map(|(idx, _)| idx.saturating_add(1)),
    );
    starts
}

fn clamp_char_boundary(source: &str, mut byte: usize) -> usize {
    byte = byte.min(source.len());
    while byte > 0 && !source.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

fn previous_grapheme_boundary(source: &str, byte: usize) -> usize {
    let byte = clamp_char_boundary(source, byte);
    source[..byte]
        .grapheme_indices(true)
        .next_back()
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

fn next_grapheme_boundary(source: &str, byte: usize) -> usize {
    let byte = clamp_char_boundary(source, byte);
    source[byte..]
        .grapheme_indices(true)
        .nth(1)
        .map(|(idx, _)| byte + idx)
        .unwrap_or(source.len())
}

fn previous_word_boundary(source: &str, byte: usize) -> usize {
    let mut cursor = clamp_char_boundary(source, byte);
    let mut class = None;
    while cursor > 0 {
        let previous = previous_grapheme_boundary(source, cursor);
        let grapheme = &source[previous..cursor];
        let next_class = word_class(grapheme);
        if class.is_none() && next_class == 0 {
            cursor = previous;
            continue;
        }
        if let Some(class) = class {
            if next_class != class {
                break;
            }
        } else {
            class = Some(next_class);
        }
        cursor = previous;
    }
    cursor
}

fn next_word_boundary(source: &str, byte: usize) -> usize {
    let mut cursor = clamp_char_boundary(source, byte);
    let mut class = None;
    while cursor < source.len() {
        let next = next_grapheme_boundary(source, cursor);
        let grapheme = &source[cursor..next];
        let next_class = word_class(grapheme);
        if class.is_none() && next_class == 0 {
            cursor = next;
            continue;
        }
        if let Some(class) = class {
            if next_class != class {
                break;
            }
        } else {
            class = Some(next_class);
        }
        cursor = next;
    }
    cursor
}

fn word_class(grapheme: &str) -> u8 {
    if grapheme.chars().all(char::is_whitespace) {
        0
    } else if grapheme.chars().any(|ch| ch.is_alphanumeric() || ch == '_') {
        1
    } else {
        2
    }
}

fn grapheme_column(text: &str) -> usize {
    text.graphemes(true).count()
}

fn byte_for_grapheme_column(text: &str, column: usize) -> usize {
    text.grapheme_indices(true)
        .nth(column)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

fn markdown_line_continuation(content: &str) -> Option<String> {
    for bullet in ["- ", "* ", "+ ", "> "] {
        if content.starts_with(bullet) {
            if content[bullet.len()..].starts_with("[ ] ")
                || content[bullet.len()..]
                    .to_ascii_lowercase()
                    .starts_with("[x] ")
            {
                return Some(format!("{bullet}[ ] "));
            }
            return Some(bullet.to_string());
        }
    }
    let digits = content.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let suffix = content.as_bytes().get(digits..digits + 2)?;
    if suffix != b". " && suffix != b") " {
        return None;
    }
    let number = content[..digits].parse::<u64>().ok()?.saturating_add(1);
    Some(format!("{number}{}", &content[digits..digits + 2]))
}

fn markdown_line_is_empty_marker(content: &str) -> bool {
    let trimmed = content.trim_end();
    if matches!(trimmed, "-" | "*" | "+" | ">")
        || matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "- [ ]" | "- [x]" | "* [ ]" | "* [x]" | "+ [ ]" | "+ [x]"
        )
    {
        return true;
    }
    let digits = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    digits > 0
        && trimmed
            .as_bytes()
            .get(digits)
            .is_some_and(|suffix| matches!(*suffix, b'.' | b')'))
        && trimmed.len() == digits + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(text: &str) -> (MarkdownDocumentSession, EditorViewState) {
        (
            MarkdownDocumentSession::new("doc".into(), PathBuf::from("Inbox.md"), text.into()),
            EditorViewState::default(),
        )
    }

    #[test]
    fn grapheme_backspace_removes_one_visible_character() {
        let (mut session, mut view) = session("a👨‍👩‍👧‍👦你");
        session.set_caret(&mut view, session.source().len(), false);
        assert!(session.backspace(&mut view));
        assert_eq!(session.source(), "a👨‍👩‍👧‍👦");
        assert!(session.backspace(&mut view));
        assert_eq!(session.source(), "a");
    }

    #[test]
    fn undo_redo_restores_source_and_selection() {
        let (mut session, mut view) = session("hello");
        session.set_caret(&mut view, 5, false);
        session.insert_text(&mut view, " 世界");
        assert_eq!(session.source(), "hello 世界");
        assert!(session.undo(&mut view));
        assert_eq!(session.source(), "hello");
        assert!(session.redo(&mut view));
        assert_eq!(session.source(), "hello 世界");
    }

    #[test]
    fn vertical_navigation_preserves_grapheme_column() {
        let (session, mut view) = session("abcd\n你😀\nabcdef");
        session.set_caret(&mut view, 3, false);
        session.move_vertical(&mut view, 1, false);
        assert_eq!(view.selection.focus.byte, "abcd\n你😀".len());
        session.move_vertical(&mut view, 1, false);
        assert_eq!(view.selection.focus.byte, "abcd\n你😀\nabc".len());
    }

    #[test]
    fn stale_save_completion_does_not_clear_newer_dirty_revision() {
        let (mut session, mut view) = session("a");
        session.set_caret(&mut view, 1, false);
        session.insert_text(&mut view, "b");
        let (revision, _, _) = session.snapshot_for_save();
        session.insert_text(&mut view, "c");
        session.finish_save(revision, Ok(()));
        assert!(session.is_dirty());
        assert_eq!(session.save_state(), &SaveState::Dirty);
    }

    #[test]
    fn formatting_surrounds_selection_and_read_only_blocks_edits() {
        let (mut session, mut view) = session("hello");
        view.selection = SourceSelection {
            anchor: SourcePosition::new(0),
            focus: SourcePosition::new(5),
        };
        assert!(session.surround_selection(&mut view, "**", "**"));
        assert_eq!(session.source(), "**hello**");
        assert_eq!(session.selected_text(&view), Some("hello"));
        view.mode = EditorMode::ReadOnly;
        assert!(!session.insert_text(&mut view, "x"));
        assert_eq!(session.source(), "**hello**");
    }

    #[test]
    fn stale_window_selection_is_clamped_after_shared_edits() {
        let (mut session, mut first_view) = session("shared");
        let mut stale_view = EditorViewState::default();
        session.set_caret(&mut stale_view, session.source().len(), false);
        session.select_all(&mut first_view);
        assert!(session.backspace(&mut first_view));
        assert_eq!(session.selected_text(&stale_view), None);
        assert!(session.insert_text(&mut stale_view, "safe"));
        assert_eq!(session.source(), "safe");
    }

    #[test]
    fn newline_continues_and_exits_markdown_lists() {
        let (mut task_session, mut view) = session("- [x] done");
        task_session.set_caret(&mut view, task_session.source().len(), false);
        assert!(task_session.insert_newline(&mut view));
        assert_eq!(task_session.source(), "- [x] done\n- [ ] ");

        let (mut ordered, mut ordered_view) = session("9. item");
        ordered.set_caret(&mut ordered_view, ordered.source().len(), false);
        assert!(ordered.insert_newline(&mut ordered_view));
        assert_eq!(ordered.source(), "9. item\n10. ");
        assert!(ordered.insert_newline(&mut ordered_view));
        assert_eq!(ordered.source(), "9. item\n");
    }

    #[test]
    fn word_navigation_and_delete_are_unicode_safe() {
        let (mut session, mut view) = session("hello 你好 👋 world");
        session.set_caret(&mut view, session.source().len(), false);
        session.move_word_left(&mut view, false);
        assert_eq!(&session.source()[view.selection.focus.byte..], "world");
        assert!(session.delete_word_back(&mut view));
        assert_eq!(session.source(), "hello 你好 world");
        session.move_word_left(&mut view, false);
        assert_eq!(&session.source()[view.selection.focus.byte..], "你好 world");
        session.move_word_right(&mut view, false);
        assert_eq!(&session.source()[view.selection.focus.byte..], " world");
    }
}
