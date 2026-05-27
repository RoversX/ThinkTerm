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
    pub selected_all: bool,
}

impl TextInputState {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            selected_all: false,
        }
    }

    pub(crate) fn push_text(&mut self, text: &str) {
        if self.selected_all {
            self.text.clear();
            self.selected_all = false;
        }
        self.text.extend(text.chars().filter(|ch| !ch.is_control()));
    }

    pub(crate) fn backspace(&mut self) {
        if self.selected_all {
            self.text.clear();
            self.selected_all = false;
        } else {
            self.text.pop();
        }
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.selected_all = false;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn select_all(&mut self) {
        self.selected_all = !self.text.is_empty();
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
