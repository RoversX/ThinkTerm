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

#[cfg(test)]
mod test {
    use super::*;
    use crate::ui::ui_scale_for_dpi;

    /// The right sidebar used to carry its own flat `6px` step with a `42px`
    /// ceiling. That reads fine at 100% and starves at 200%, where a row is
    /// twice as tall but the step is not: one notch moved well under half a
    /// row while the same notch in the settings window moved several. Both
    /// surfaces now come through here, so a notch has to keep tracking dpi.
    #[test]
    fn a_wheel_notch_covers_the_same_rows_at_every_dpi() {
        let at_96 = wheel_delta_pixels(1, ui_scale_for_dpi(96));
        let at_192 = wheel_delta_pixels(1, ui_scale_for_dpi(192));

        assert!(
            (at_192 - at_96 * 2.0).abs() < 0.01,
            "doubling the dpi must double the notch: {at_96} -> {at_192}"
        );
        assert!(
            at_96 < 0.0,
            "a positive wheel amount scrolls towards the top of the content"
        );
        assert!(
            (wheel_delta_pixels(-1, ui_scale_for_dpi(96)) + at_96).abs() < f32::EPSILON,
            "reversing the wheel must reverse the distance exactly"
        );
    }

    /// The old sidebar helper clamped a whole event to 42px, which at its 6px
    /// step meant anything past 7 notches was silently discarded. Windows
    /// folds the user's scroll-lines setting into one event, so a fast flick
    /// or a raised `WheelScrollLines` lands well past that. Distance has to
    /// stay proportional to the notches the event actually carries.
    #[test]
    fn a_multi_notch_event_is_not_clamped() {
        let scale = ui_scale_for_dpi(192);
        let one = wheel_delta_pixels(1, scale);
        // 10 notches: past where the old 42px ceiling (7 notches at 6px) bit.
        let ten = wheel_delta_pixels(10, scale);

        assert!(
            (ten - one * 10.0).abs() < 0.01,
            "ten notches must move ten times as far: {one} -> {ten}"
        );
    }
}
