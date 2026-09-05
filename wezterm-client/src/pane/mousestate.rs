use wezterm_term::{MouseButton, MouseEvent, MouseEventKind};

/// The GUI passes through the raw OS wheel amount (lines per notch), but
/// a local terminal emits exactly one report per wheel event no matter
/// the amount. Normalize each event to a single notch so that coalescing
/// accumulates a notch count; the mux server replays one report per
/// notch, matching local behavior.
pub fn normalize_wheel(mut event: MouseEvent) -> MouseEvent {
    event.button = match event.button {
        MouseButton::WheelUp(n) if n > 1 => MouseButton::WheelUp(1),
        MouseButton::WheelDown(n) if n > 1 => MouseButton::WheelDown(1),
        MouseButton::WheelLeft(n) if n > 1 => MouseButton::WheelLeft(1),
        MouseButton::WheelRight(n) if n > 1 => MouseButton::WheelRight(1),
        b => b,
    };
    event
}

/// Fold `event` into `last`, the mouse event queued right before it, when
/// the two can travel as one: interim moves collapse into the latest,
/// repeated wheel notches add up. Hands `event` back when it has to stay
/// its own report.
pub fn coalesce(last: &mut MouseEvent, event: MouseEvent) -> Option<MouseEvent> {
    if last.modifiers != event.modifiers {
        return Some(event);
    }
    if last.kind == MouseEventKind::Move
        && event.kind == MouseEventKind::Move
        && last.button == event.button
    {
        *last = event;
        return None;
    }
    match (&last.button, &event.button) {
        (MouseButton::WheelUp(a), MouseButton::WheelUp(b)) => {
            last.button = MouseButton::WheelUp(a + b);
            None
        }
        (MouseButton::WheelDown(a), MouseButton::WheelDown(b)) => {
            last.button = MouseButton::WheelDown(a + b);
            None
        }
        (MouseButton::WheelLeft(a), MouseButton::WheelLeft(b)) => {
            last.button = MouseButton::WheelLeft(a + b);
            None
        }
        (MouseButton::WheelRight(a), MouseButton::WheelRight(b)) => {
            last.button = MouseButton::WheelRight(a + b);
            None
        }
        _ => Some(event),
    }
}
