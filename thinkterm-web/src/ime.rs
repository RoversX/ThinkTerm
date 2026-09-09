//! Position of the browser's IME input field, in viewport CSS pixels.
//! The terminal uses device pixels and stable scrollback rows instead.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchor {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

/// Keep the input field inside the visible canvas even when the terminal
/// cursor is in a row scrolled out of view or just past the right margin.
/// `cursor` and `cell` are device pixels, relative to the canvas.
pub fn anchor(
    rect: [f64; 4],
    surface: (u32, u32),
    cursor: (f64, f64),
    cell: (f64, f64),
) -> Option<Anchor> {
    let [left, top, width, height] = rect;
    if surface.0 == 0 || surface.1 == 0 || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let sx = width / surface.0 as f64;
    let sy = height / surface.1 as f64;
    let w = (cell.0 * sx).max(1.0).min(width);
    let h = (cell.1 * sy).max(1.0).min(height);
    Some(Anchor {
        left: left + (cursor.0 * sx).clamp(0.0, width - w),
        top: top + (cursor.1 * sy).clamp(0.0, height - h),
        width: w,
        height: h,
    })
}

/// Update only changed geometry, without touching focus, composition,
/// value or selection. Returns whether anything changed for the smoke probe.
#[cfg(target_arch = "wasm32")]
pub fn update_field(
    field: &web_sys::HtmlTextAreaElement,
    previous: &mut Option<Anchor>,
    next: Option<Anchor>,
) -> Result<bool, wasm_bindgen::JsValue> {
    let Some(next) = next.filter(|a| Some(*a) != *previous) else {
        return Ok(false);
    };
    let style = field.style();
    if previous.is_none_or(|old| old.left != next.left) {
        style.set_property("left", &format!("{}px", next.left))?;
    }
    if previous.is_none_or(|old| old.top != next.top) {
        style.set_property("top", &format!("{}px", next.top))?;
    }
    if previous.is_none_or(|old| old.width != next.width || old.height != next.height) {
        style.set_property("width", &format!("{}px", next.width))?;
        style.set_property("height", &format!("{}px", next.height))?;
        style.set_property("line-height", &format!("{}px", next.height))?;
        style.set_property("font-size", &format!("{}px", next.height * 0.75))?;
    }
    *previous = Some(next);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_pixels_become_css_pixels_at_the_canvas_origin() {
        assert_eq!(
            anchor(
                [30.0, 40.0, 800.0, 600.0],
                (1600, 1200),
                (200.0, 172.0),
                (20.0, 43.0)
            ),
            Some(Anchor {
                left: 130.0,
                top: 126.0,
                width: 10.0,
                height: 21.5
            })
        );
    }

    #[test]
    fn fractional_zoom_and_double_width_rows_use_actual_surface_scale() {
        let a = anchor(
            [0.0, 0.0, 803.0, 601.0],
            (1003, 751),
            (40.0, 86.0),
            (40.0, 86.0),
        )
        .unwrap();
        assert!((a.left - 40.0 * 803.0 / 1003.0).abs() < 1e-9);
        assert!((a.height - 86.0 * 601.0 / 751.0).abs() < 1e-9);
    }

    #[test]
    fn scrollback_and_right_margin_do_not_move_the_ime_outside_the_canvas() {
        let rect = [10.0, 20.0, 800.0, 600.0];
        let bottom = anchor(rect, (800, 600), (900.0, 9000.0), (10.0, 22.0)).unwrap();
        assert_eq!(
            bottom,
            Anchor {
                left: 800.0,
                top: 598.0,
                width: 10.0,
                height: 22.0
            }
        );
        let top = anchor(rect, (800, 600), (0.0, -22.0), (10.0, 22.0)).unwrap();
        assert_eq!(top.top, 20.0);
        assert_eq!(anchor(rect, (0, 600), (0.0, 0.0), (10.0, 22.0)), None);
        assert_eq!(anchor([0.0; 4], (800, 600), (0.0, 0.0), (10.0, 22.0)), None);
    }
}
