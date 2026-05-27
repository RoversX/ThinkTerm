pub(crate) fn rect(x: f32, y: f32, width: f32, height: f32) -> window::RectF {
    euclid::rect(x, y, width.max(0.0), height.max(0.0))
}

pub(crate) fn inset(rect: window::RectF, amount: f32) -> window::RectF {
    euclid::rect(
        rect.origin.x + amount,
        rect.origin.y + amount,
        (rect.size.width - amount * 2.0).max(0.0),
        (rect.size.height - amount * 2.0).max(0.0),
    )
}

pub(crate) fn contains(rect: window::RectF, x: f32, y: f32) -> bool {
    rect.contains(euclid::point2(x, y))
}

pub(crate) fn clamp(value: f32, min: f32, max: f32) -> f32 {
    value.max(min).min(max)
}
