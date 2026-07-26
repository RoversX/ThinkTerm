use crate::ui::{contains, ScrollState};
use window::{MouseEvent, MouseEventKind};

/// Pixels moved per wheel notch. `scale` is the design-pixel ratio
/// ([`DrawContext::scale`]): rows shrink with the UI, so the scroll step has to
/// shrink with them or one notch jumps a different number of rows per display.
pub(crate) fn wheel_delta_pixels(amount: i16, scale: f32) -> f32 {
    -(amount as f32) * WHEEL_NOTCH_PIXELS * scale
}

/// Distance a single wheel notch scrolls, in design pixels.
const WHEEL_NOTCH_PIXELS: f32 = 34.0;

pub(crate) fn precise_wheel_delta_pixels(event: &MouseEvent) -> Option<f32> {
    let delta = event.precise_scroll_delta?;
    if delta.y.abs() >= delta.x.abs() && delta.y.abs() > f32::EPSILON {
        Some(-delta.y)
    } else {
        None
    }
}

pub(crate) fn apply_wheel_to_area(
    event: &MouseEvent,
    area: window::RectF,
    scroll: &mut ScrollState,
    scale: f32,
) -> bool {
    if !contains(area, event.coords.x as f32, event.coords.y as f32) {
        return false;
    }

    scroll.set_phase(event.momentum_phase.or(event.scroll_phase));

    if let Some(delta) = precise_wheel_delta_pixels(event) {
        let old = scroll.offset;
        scroll.scroll_by(delta);
        return (scroll.offset - old).abs() > 0.01;
    }

    match event.kind {
        MouseEventKind::VertWheel(amount) => {
            let old_velocity = scroll.velocity;
            scroll.scroll_by_smooth(wheel_delta_pixels(amount, scale));
            (scroll.velocity - old_velocity).abs() > 0.5
        }
        _ => false,
    }
}
