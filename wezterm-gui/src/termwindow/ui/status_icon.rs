use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::termwindow::ui::icons::SvgIcon;
use crate::termwindow::TermWindow;
use anyhow::Context;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wezterm_term::Progress;
use window::color::LinearRgba;

const SPINNER_FRAME_COUNT: u8 = 12;
const SPINNER_FRAME_MS: u64 = 80;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiStatusKind {
    Running,
    NeedsAttention,
    Done,
}

impl UiStatusKind {
    pub fn from_progress(progress: &Progress) -> Option<Self> {
        match progress {
            Progress::None => None,
            Progress::Percentage(_) | Progress::Indeterminate => Some(Self::Running),
            Progress::Error(_) => Some(Self::NeedsAttention),
        }
    }

    pub fn icon(self) -> SvgIcon {
        match self {
            Self::Running => SvgIcon::LoaderCircle,
            Self::NeedsAttention => SvgIcon::CircleAlert,
            Self::Done => SvgIcon::CircleCheck,
        }
    }

    pub fn is_spinning(self) -> bool {
        matches!(self, Self::Running)
    }
}

pub fn split_leading_legacy_progress_marker(title: &str) -> Option<&str> {
    let trimmed = title.trim_start();
    let mut chars = trimmed.char_indices();
    let (_, ch) = chars.next()?;
    if !is_legacy_progress_marker_char(ch) {
        return None;
    }

    let marker_end = chars.next().map(|(idx, _)| idx).unwrap_or(trimmed.len());
    Some(trimmed[marker_end..].trim_start())
}

fn is_legacy_progress_marker_char(ch: char) -> bool {
    // 0x25d0-0x25d3 are the half-circle busy spinner frames Claude Code
    // switched to in 2.1.228 (Braille before that). U+2733 ✳ is deliberately
    // absent: Claude uses it as the *idle* title marker.
    matches!(
        ch as u32,
        0x2800..=0x28ff | 0x25d0..=0x25d3 | 0xf0130 | 0xf0a9e..=0xf0aa5 | 0xee00..=0xee0b
    )
}

impl TermWindow {
    pub(crate) fn paint_ui_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer: usize,
        icon: SvgIcon,
        x: usize,
        y: usize,
        size: usize,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        self.paint_ui_icon_impl(layers, layer, icon, x, y, size, color, None)
    }

    pub(crate) fn paint_spinning_ui_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer: usize,
        icon: SvgIcon,
        x: usize,
        y: usize,
        size: usize,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let frame =
            ((now.as_millis() / SPINNER_FRAME_MS as u128) % SPINNER_FRAME_COUNT as u128) as u8;
        self.update_next_frame_time(Some(
            Instant::now() + Duration::from_millis(SPINNER_FRAME_MS),
        ));
        self.paint_ui_icon_impl(layers, layer, icon, x, y, size, color, Some(frame))
    }

    pub(crate) fn paint_status_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer: usize,
        status: UiStatusKind,
        x: usize,
        y: usize,
        size: usize,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        if status.is_spinning() {
            self.paint_spinning_ui_icon(layers, layer, status.icon(), x, y, size, color)
        } else {
            self.paint_ui_icon(layers, layer, status.icon(), x, y, size, color)
        }
    }

    fn paint_ui_icon_impl(
        &self,
        layers: &mut TripleLayerQuadAllocator,
        layer: usize,
        icon: SvgIcon,
        x: usize,
        y: usize,
        size: usize,
        color: LinearRgba,
        rotation_frame: Option<u8>,
    ) -> anyhow::Result<()> {
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        let gl_state = self.render_state.as_ref().unwrap();
        let sprite = if let Some(frame) = rotation_frame {
            gl_state.glyph_cache.borrow_mut().cached_rotated_svg_icon(
                icon,
                size,
                frame,
                SPINNER_FRAME_COUNT,
            )?
        } else {
            gl_state
                .glyph_cache
                .borrow_mut()
                .cached_svg_icon(icon, size)?
        }
        .texture_coords();

        let mut quad = layers.allocate(layer).context("allocate ui icon quad")?;
        quad.set_position(
            x as f32 - left_offset,
            y as f32 - top_offset,
            x as f32 + size as f32 - left_offset,
            y as f32 + size as f32 - top_offset,
        );
        quad.set_texture(sprite);
        quad.set_fg_color(color);
        quad.set_alt_color_and_mix_value(color, 0.0);
        quad.set_hsv(None);
        quad.set_grayscale();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::split_leading_legacy_progress_marker;

    #[test]
    fn braille_spinner_splits() {
        assert_eq!(
            split_leading_legacy_progress_marker("⠋ build"),
            Some("build")
        );
    }

    #[test]
    fn half_circle_spinner_splits() {
        // Claude Code >= 2.1.228 busy spinner frames.
        for frame in ['◐', '◑', '◒', '◓'] {
            let title = format!("{frame} fix the tests");
            assert_eq!(
                split_leading_legacy_progress_marker(&title),
                Some("fix the tests"),
                "frame {frame:?}"
            );
        }
    }

    #[test]
    fn nerd_font_and_private_use_markers_split() {
        assert_eq!(
            split_leading_legacy_progress_marker("\u{f0130} task"),
            Some("task")
        );
        assert_eq!(
            split_leading_legacy_progress_marker("\u{ee03} task"),
            Some("task")
        );
    }

    #[test]
    fn idle_marker_and_plain_text_do_not_split() {
        // ✳ is Claude's *idle* title marker; treating it as a busy spinner
        // would invert the state.
        assert_eq!(split_leading_legacy_progress_marker("✳ done"), None);
        assert_eq!(split_leading_legacy_progress_marker("zsh"), None);
        assert_eq!(split_leading_legacy_progress_marker(""), None);
    }
}
