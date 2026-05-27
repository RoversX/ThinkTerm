use crate::ui::{contains, ScrollState};
use window::{MouseEvent, MouseEventKind};

pub(crate) fn wheel_delta_pixels(amount: i16) -> f32 {
    -(amount as f32) * 34.0
}

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
            scroll.scroll_by_smooth(wheel_delta_pixels(amount));
            (scroll.velocity - old_velocity).abs() > 0.5
        }
        _ => false,
    }
}
