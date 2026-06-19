use std::time::Instant;
use window::ScrollPhase;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct WidgetId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WidgetKind {
    Button,
    TextInput,
    SidebarRow,
    ResizeHandle,
    ScrollArea,
    PreviewControl,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct HitTarget<A: Copy> {
    pub rect: window::RectF,
    pub kind: WidgetKind,
    pub action: A,
}

#[derive(Debug, Clone)]
pub(crate) struct UiContext<A: Copy> {
    hits: Vec<HitTarget<A>>,
}

impl<A: Copy> Default for UiContext<A> {
    fn default() -> Self {
        Self { hits: Vec::new() }
    }
}

impl<A: Copy> UiContext<A> {
    pub(crate) fn clear(&mut self) {
        self.hits.clear();
    }

    pub(crate) fn push(&mut self, rect: window::RectF, kind: WidgetKind, action: A) {
        self.hits.push(HitTarget { rect, kind, action });
    }

    pub(crate) fn hit_test(&self, x: f32, y: f32) -> Option<HitTarget<A>> {
        self.hits
            .iter()
            .rev()
            .find(|target| target.rect.contains(euclid::point2(x, y)))
            .copied()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InteractionState<A: Copy + PartialEq> {
    pub hovered: Option<A>,
    pub pressed: Option<A>,
    pub focused: Option<A>,
}

impl<A: Copy + PartialEq> Default for InteractionState<A> {
    fn default() -> Self {
        Self {
            hovered: None,
            pressed: None,
            focused: None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TextInputState {
    pub text: String,
    /// Cached "whole text selected" flag. Kept in sync by every mutator so the
    /// simple consumers (settings window, widgets) can keep reading it, while
    /// the sidebar uses the richer caret/selection model below.
    pub selected_all: bool,
    /// Caret position as a char index in `0..=char_len()`.
    pub cursor: usize,
    /// When `Some`, there is a selection spanning `selection_anchor..cursor`
    /// (char indices, either order).
    pub selection_anchor: Option<usize>,
}

impl TextInputState {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            selected_all: false,
            cursor: 0,
            selection_anchor: None,
        }
    }

    // -----------------------------------------------------------------------
    // Legacy append-at-end API. The settings window prefills `.text` directly
    // and never moves a caret, so these must keep their original behaviour.
    // -----------------------------------------------------------------------

    pub(crate) fn push_text(&mut self, text: &str) {
        if self.selected_all {
            self.text.clear();
            self.selected_all = false;
        }
        self.text.extend(text.chars().filter(|ch| !ch.is_control()));
        self.cursor = self.char_len();
        self.selection_anchor = None;
    }

    pub(crate) fn backspace(&mut self) {
        if self.selected_all {
            self.text.clear();
            self.selected_all = false;
        } else {
            self.text.pop();
        }
        self.cursor = self.char_len();
        self.selection_anchor = None;
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.selected_all = false;
        self.cursor = 0;
        self.selection_anchor = None;
    }

    pub(crate) fn take_selected_text(&mut self) -> Option<String> {
        if self.selected_all && !self.text.is_empty() {
            self.selected_all = false;
            self.cursor = 0;
            self.selection_anchor = None;
            Some(std::mem::take(&mut self.text))
        } else {
            None
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn select_all(&mut self) {
        self.selected_all = !self.text.is_empty();
        if self.selected_all {
            self.selection_anchor = Some(0);
            self.cursor = self.char_len();
        } else {
            self.selection_anchor = None;
            self.cursor = 0;
        }
    }

    // -----------------------------------------------------------------------
    // Caret/selection editor API (single-line sidebar inputs).
    // -----------------------------------------------------------------------

    pub(crate) fn char_len(&self) -> usize {
        self.text.chars().count()
    }

    pub(crate) fn byte_idx_for(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(idx, _)| idx)
            .unwrap_or_else(|| self.text.len())
    }

    /// Replace the text and place the caret at the end (used when an editor is
    /// pre-populated with existing content).
    pub(crate) fn set_text_end(&mut self, text: String) {
        self.text = text;
        self.cursor = self.char_len();
        self.selection_anchor = None;
        self.selected_all = false;
    }

    fn sync_selected_all(&mut self) {
        self.selected_all =
            !self.text.is_empty() && self.caret_selection_range() == Some((0, self.char_len()));
    }

    pub(crate) fn clear_selection(&mut self) {
        self.selected_all = false;
        self.selection_anchor = None;
    }

    pub(crate) fn caret_selection_range(&self) -> Option<(usize, usize)> {
        let len = self.char_len();
        let anchor = self.selection_anchor?.min(len);
        let cursor = self.cursor.min(len);
        if anchor == cursor {
            None
        } else if anchor < cursor {
            Some((anchor, cursor))
        } else {
            Some((cursor, anchor))
        }
    }

    pub(crate) fn caret_selected_text(&self) -> Option<String> {
        let (start, end) = self.caret_selection_range()?;
        Some(self.text.chars().skip(start).take(end - start).collect())
    }

    pub(crate) fn caret_delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.caret_selection_range() else {
            return false;
        };
        let start_byte = self.byte_idx_for(start);
        let end_byte = self.byte_idx_for(end);
        self.text.replace_range(start_byte..end_byte, "");
        self.cursor = start;
        self.selection_anchor = None;
        self.selected_all = false;
        true
    }

    pub(crate) fn caret_insert(&mut self, text: &str, allow_newline: bool) {
        self.caret_delete_selection();
        let len = self.char_len();
        if self.cursor > len {
            self.cursor = len;
        }
        let mut byte = self.byte_idx_for(self.cursor);
        let mut inserted = 0usize;
        for ch in text.chars() {
            let keep = if ch == '\n' || ch == '\t' {
                allow_newline
            } else {
                !ch.is_control()
            };
            if !keep {
                continue;
            }
            self.text.insert(byte, ch);
            byte += ch.len_utf8();
            inserted += 1;
        }
        self.cursor += inserted;
        self.selection_anchor = None;
        self.selected_all = false;
    }

    pub(crate) fn caret_backspace(&mut self) {
        if self.caret_delete_selection() {
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let remove = self.cursor - 1;
        let start_byte = self.byte_idx_for(remove);
        let end_byte = self.byte_idx_for(self.cursor);
        self.text.replace_range(start_byte..end_byte, "");
        self.cursor = remove;
        self.selected_all = false;
    }

    pub(crate) fn caret_delete_forward(&mut self) {
        if self.caret_delete_selection() {
            return;
        }
        let len = self.char_len();
        if self.cursor >= len {
            return;
        }
        let start_byte = self.byte_idx_for(self.cursor);
        let end_byte = self.byte_idx_for(self.cursor + 1);
        self.text.replace_range(start_byte..end_byte, "");
        self.selected_all = false;
    }

    pub(crate) fn caret_set(&mut self, char_idx: usize, extend: bool) {
        let target = char_idx.min(self.char_len());
        if extend {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.selection_anchor = None;
        }
        self.cursor = target;
        self.sync_selected_all();
    }

    pub(crate) fn caret_move_left(&mut self, extend: bool) {
        if !extend {
            if let Some((start, _)) = self.caret_selection_range() {
                self.cursor = start;
                self.selection_anchor = None;
                self.selected_all = false;
                return;
            }
        }
        let target = self.cursor.saturating_sub(1);
        self.caret_set(target, extend);
    }

    pub(crate) fn caret_move_right(&mut self, extend: bool) {
        if !extend {
            if let Some((_, end)) = self.caret_selection_range() {
                self.cursor = end;
                self.selection_anchor = None;
                self.selected_all = false;
                return;
            }
        }
        let target = self.cursor.saturating_add(1).min(self.char_len());
        self.caret_set(target, extend);
    }

    pub(crate) fn caret_move_home(&mut self, extend: bool) {
        self.caret_set(0, extend);
    }

    pub(crate) fn caret_move_end(&mut self, extend: bool) {
        self.caret_set(self.char_len(), extend);
    }

    pub(crate) fn caret_select_all(&mut self) {
        if self.text.is_empty() {
            self.selection_anchor = None;
            self.cursor = 0;
            self.selected_all = false;
            return;
        }
        self.selection_anchor = Some(0);
        self.cursor = self.char_len();
        self.selected_all = true;
    }

    pub(crate) fn caret_take_selected_text(&mut self) -> Option<String> {
        let text = self.caret_selected_text()?;
        self.caret_delete_selection();
        Some(text)
    }

    fn prev_word_boundary(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let mut idx = self.cursor.min(chars.len());
        while idx > 0 && chars[idx - 1].is_whitespace() {
            idx -= 1;
        }
        while idx > 0 && !chars[idx - 1].is_whitespace() {
            idx -= 1;
        }
        idx
    }

    fn next_word_boundary(&self) -> usize {
        let chars: Vec<char> = self.text.chars().collect();
        let len = chars.len();
        let mut idx = self.cursor.min(len);
        while idx < len && chars[idx].is_whitespace() {
            idx += 1;
        }
        while idx < len && !chars[idx].is_whitespace() {
            idx += 1;
        }
        idx
    }

    pub(crate) fn caret_word_left(&mut self, extend: bool) {
        let target = self.prev_word_boundary();
        self.caret_set(target, extend);
    }

    pub(crate) fn caret_word_right(&mut self, extend: bool) {
        let target = self.next_word_boundary();
        self.caret_set(target, extend);
    }

    pub(crate) fn caret_delete_word_back(&mut self) {
        if self.caret_delete_selection() {
            return;
        }
        let target = self.prev_word_boundary();
        if target < self.cursor {
            let start_byte = self.byte_idx_for(target);
            let end_byte = self.byte_idx_for(self.cursor);
            self.text.replace_range(start_byte..end_byte, "");
            self.cursor = target;
        }
        self.selected_all = false;
    }

    pub(crate) fn caret_delete_to_start(&mut self) {
        if self.caret_delete_selection() {
            return;
        }
        if self.cursor > 0 {
            let end_byte = self.byte_idx_for(self.cursor);
            self.text.replace_range(0..end_byte, "");
            self.cursor = 0;
        }
        self.selected_all = false;
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ScrollState {
    pub offset: f32,
    pub velocity: f32,
    pub target_offset: f32,
    pub viewport_extent: f32,
    pub content_extent: f32,
    pub active_phase: Option<ScrollPhase>,
    last_animation_time: Option<Instant>,
}

impl ScrollState {
    pub(crate) fn new() -> Self {
        Self {
            offset: 0.0,
            velocity: 0.0,
            target_offset: 0.0,
            viewport_extent: 0.0,
            content_extent: 0.0,
            active_phase: None,
            last_animation_time: None,
        }
    }

    pub(crate) fn set_extents(&mut self, viewport_extent: f32, content_extent: f32) {
        self.viewport_extent = viewport_extent.max(0.0);
        self.content_extent = content_extent.max(0.0);
        self.offset = self.offset.clamp(0.0, self.max_offset());
        self.target_offset = self.target_offset.clamp(0.0, self.max_offset());
    }

    pub(crate) fn scroll_by(&mut self, delta: f32) {
        let offset = (self.offset + delta).clamp(0.0, self.max_offset());
        self.offset = offset;
        self.target_offset = offset;
        self.velocity = 0.0;
        self.last_animation_time = None;
    }

    pub(crate) fn scroll_by_smooth(&mut self, delta: f32) {
        self.velocity += delta * 24.0;
        self.target_offset = self.offset;
        self.last_animation_time = None;
    }

    pub(crate) fn set_phase(&mut self, phase: Option<ScrollPhase>) {
        self.active_phase = phase;
        if matches!(phase, Some(ScrollPhase::Ended | ScrollPhase::Cancelled)) {
            self.active_phase = None;
        }
    }

    pub(crate) fn reset(&mut self) {
        self.offset = 0.0;
        self.target_offset = 0.0;
        self.velocity = 0.0;
        self.active_phase = None;
        self.last_animation_time = None;
    }

    pub(crate) fn advance_animation(&mut self, now: Instant) -> bool {
        let previous = self.last_animation_time.replace(now).unwrap_or(now);
        let dt = now
            .saturating_duration_since(previous)
            .as_secs_f32()
            .clamp(1.0 / 240.0, 1.0 / 30.0);

        if self.velocity.abs() > 0.5 {
            let old = self.offset;
            self.offset = (self.offset + self.velocity * dt).clamp(0.0, self.max_offset());
            if (self.offset - old).abs() <= f32::EPSILON
                && (self.offset <= 0.0 || self.offset >= self.max_offset())
            {
                self.velocity = 0.0;
            } else {
                self.velocity *= (-18.0 * dt).exp();
            }
            self.target_offset = self.offset;
            return true;
        }

        self.velocity = 0.0;

        let target_delta = self.target_offset - self.offset;
        if target_delta.abs() <= 0.45 {
            let was_animating = (self.offset - self.target_offset).abs() > f32::EPSILON;
            self.offset = self.target_offset;
            self.last_animation_time = None;
            return was_animating;
        }

        let alpha = 1.0 - (-42.0 * dt).exp();
        self.offset += target_delta * alpha;
        true
    }

    pub(crate) fn max_offset(&self) -> f32 {
        (self.content_extent - self.viewport_extent).max(0.0)
    }

    pub(crate) fn has_overflow(&self) -> bool {
        self.max_offset() > 0.5
    }

    pub(crate) fn thumb(&self, track_start: f32, track_extent: f32) -> Option<(f32, f32)> {
        if !self.has_overflow() || track_extent <= 0.0 || self.content_extent <= 0.0 {
            return None;
        }
        let ratio = (self.viewport_extent / self.content_extent).clamp(0.08, 1.0);
        let thumb_extent = (track_extent * ratio).max(32.0).min(track_extent);
        let travel = (track_extent - thumb_extent).max(0.0);
        let progress = if self.max_offset() <= 0.0 {
            0.0
        } else {
            self.offset / self.max_offset()
        };
        Some((track_start + travel * progress, thumb_extent))
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ResizablePaneState {
    pub width: f32,
    pub min_width: f32,
    pub max_width: f32,
}

impl ResizablePaneState {
    pub(crate) fn new(width: f32, min_width: f32, max_width: f32) -> Self {
        Self {
            width: width.clamp(min_width, max_width),
            min_width,
            max_width,
        }
    }

    pub(crate) fn set_width(&mut self, width: f32) {
        self.width = width.clamp(self.min_width, self.max_width);
    }
}

#[cfg(test)]
mod text_input_tests {
    use super::TextInputState;

    fn input(text: &str) -> TextInputState {
        let mut input = TextInputState::new();
        input.set_text_end(text.to_string());
        input
    }

    #[test]
    fn inserts_at_the_caret() {
        let mut i = input("helloworld");
        i.caret_set(5, false);
        i.caret_insert(", ", false);
        assert_eq!(i.text, "hello, world");
        assert_eq!(i.cursor, 7);
    }

    #[test]
    fn backspace_and_delete_forward_at_caret() {
        let mut i = input("abc");
        i.caret_set(2, false);
        i.caret_backspace();
        assert_eq!(i.text, "ac");
        assert_eq!(i.cursor, 1);
        i.caret_delete_forward();
        assert_eq!(i.text, "a");
        assert_eq!(i.cursor, 1);
    }

    #[test]
    fn selection_is_replaced_on_insert() {
        let mut i = input("hello world");
        i.caret_set(0, false);
        i.caret_set(5, true);
        assert_eq!(i.caret_selected_text().as_deref(), Some("hello"));
        i.caret_insert("hi", false);
        assert_eq!(i.text, "hi world");
        assert_eq!(i.cursor, 2);
        assert!(i.caret_selection_range().is_none());
    }

    #[test]
    fn clear_selection_drops_caret_selection() {
        let mut i = input("hello world");
        i.caret_set(0, false);
        i.caret_set(5, true);
        assert_eq!(i.caret_selection_range(), Some((0, 5)));

        i.clear_selection();
        assert!(!i.selected_all);
        assert!(i.caret_selection_range().is_none());

        i.caret_insert("hi", false);
        assert_eq!(i.text, "hellohi world");
    }

    #[test]
    fn stale_selection_range_is_clamped_to_current_text() {
        let mut i = input("abc");
        i.cursor = 0;
        i.selection_anchor = Some(4);

        assert_eq!(i.caret_selection_range(), Some((0, 3)));
        assert_eq!(i.caret_selected_text().as_deref(), Some("abc"));
    }

    #[test]
    fn plain_move_collapses_selection_to_an_edge() {
        let mut i = input("abcd");
        i.caret_set(1, false);
        i.caret_set(3, true);
        i.caret_move_left(false);
        assert_eq!(i.cursor, 1);
        assert!(i.caret_selection_range().is_none());
    }

    #[test]
    fn select_all_then_typing_replaces_everything() {
        let mut i = input("replace me");
        i.caret_select_all();
        assert!(i.selected_all);
        i.caret_insert("x", false);
        assert_eq!(i.text, "x");
        assert!(!i.selected_all);
    }

    #[test]
    fn word_navigation_and_word_delete() {
        let mut i = input("foo bar baz");
        i.caret_move_end(false);
        i.caret_word_left(false);
        assert_eq!(i.cursor, 8);
        i.caret_delete_word_back();
        assert_eq!(i.text, "foo baz");
        assert_eq!(i.cursor, 4);
    }

    #[test]
    fn caret_respects_multibyte_chars() {
        let mut i = input("aé中b");
        i.caret_set(2, false);
        i.caret_insert("X", false);
        assert_eq!(i.text, "aéX中b");
        assert_eq!(i.cursor, 3);
        i.caret_backspace();
        assert_eq!(i.text, "aé中b");
        assert_eq!(i.cursor, 2);
    }

    #[test]
    fn caret_set_clamps_to_text_length() {
        let mut i = input("ab");
        i.caret_set(99, false);
        assert_eq!(i.cursor, 2);
    }
}
