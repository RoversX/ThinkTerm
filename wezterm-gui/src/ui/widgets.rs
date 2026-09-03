//! Reusable, action-generic UI controls drawn on top of [`DrawContext`].
//!
//! These mirror the control-drawing helpers in `settings_window.rs` but are
//! parameterised over the caller's action type `A` and take the interaction /
//! palette / hit-test context explicitly, so any native surface can use them.

use crate::quad::TripleLayerQuadAllocator;
use crate::termwindow::ui::icons::SvgIcon;
use crate::ui::{
    ButtonSpec, DrawContext, InteractionState, ScrollState, ScrollbarSpec, TextInputSpec,
    UiContext, UiPalette, UiTokens, WidgetKind,
};
use std::rc::Rc;
use wezterm_font::LoadedFont;
use window::color::LinearRgba;
use window::RectF;

/// Geometry below is authored in *design pixels* (the 2x macOS backing grid)
/// and must be converted with [`DrawContext::px`] at the point of use, exactly
/// like the callers size their rects. Mixing a scaled rect with an unscaled
/// inset silently breaks every display whose scale is not 1.0 — see
/// `ui::tokens::ui_scale_for_dpi`.
///
/// Horizontal inset for a button label; the available text width is the button
/// width minus this on *both* sides. Callers must therefore size buttons as
/// `measure_text_width(label) + px(N)` with `N` at least twice this, or the
/// widget will ellipsize the label it was asked to show. `px(56)` is the
/// house size and leaves 16 design px of slack.
const BUTTON_TEXT_PAD: f32 = 20.0;
const ICON_BUTTON_RADIUS: f32 = 8.0;
/// Icon side length is derived from the (already DPI-aware) cell height, so
/// only the padding and the clamp bounds are design pixels.
const ICON_CELL_PADDING: f32 = 4.0;
const ICON_MIN_SIZE: f32 = 20.0;
const ICON_MAX_SIZE: f32 = 30.0;
/// Selection highlight bleeds this far outside the text box's text area.
const INPUT_SELECTION_BLEED_X: f32 = 4.0;
/// Vertical inset of the selection highlight / caret inside the text box.
const INPUT_SELECTION_INSET_Y: f32 = 5.0;
const INPUT_SELECTION_RADIUS_INSET: f32 = 5.0;
const INPUT_SELECTION_MIN_RADIUS: f32 = 3.0;
const CARET_WIDTH: f32 = 3.0;
/// Gap between the toggle track edge and its knob.
const TOGGLE_KNOB_INSET: f32 = 2.0;

/// Vertically center single-line text of the UI font within `height`.
fn control_text_y(ctx: &DrawContext, y: f32, height: f32) -> f32 {
    let cell_height = ctx.metrics.cell_size.height as f32;
    y + ((height - cell_height) / 2.0).max(0.0)
}

/// Width available for a button's label inside `button_width`. Split out so the
/// pairing with [`BUTTON_TEXT_PAD`] can be unit-tested without a GPU context.
pub(crate) fn button_label_width(button_width: f32, scale: f32) -> f32 {
    button_width - BUTTON_TEXT_PAD * 2.0 * scale
}

/// A filled pill button with a centred label. The caller fills `spec.state`
/// (e.g. from its [`InteractionState`]) and `spec.variant`.
pub(crate) fn draw_button<A: Copy + PartialEq>(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    font: &Rc<LoadedFont>,
    widgets: &mut UiContext<A>,
    palette: UiPalette,
    spec: ButtonSpec<A>,
) -> anyhow::Result<()> {
    draw_button_with_layers(ctx, layers, font, widgets, palette, spec, 0, 1)
}

/// Draw the complete button on a single explicit layer. Full-window views use
/// this for confirmation controls that must sit above embedded terminal glyphs
/// while retaining the same shared geometry and interaction styling.
pub(crate) fn draw_button_on_layer<A: Copy + PartialEq>(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    font: &Rc<LoadedFont>,
    widgets: &mut UiContext<A>,
    palette: UiPalette,
    spec: ButtonSpec<A>,
    layer_num: usize,
) -> anyhow::Result<()> {
    draw_button_with_layers(
        ctx, layers, font, widgets, palette, spec, layer_num, layer_num,
    )
}

#[allow(clippy::too_many_arguments)]
fn draw_button_with_layers<A: Copy + PartialEq>(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    font: &Rc<LoadedFont>,
    widgets: &mut UiContext<A>,
    palette: UiPalette,
    spec: ButtonSpec<A>,
    background_layer: usize,
    text_layer: usize,
) -> anyhow::Result<()> {
    widgets.push(spec.rect, spec.kind, spec.action);
    let (background, border, label_color) = spec.variant.colors(spec.state, palette);
    ctx.draw_rounded_frame(
        layers,
        background_layer,
        spec.rect.origin.x,
        spec.rect.origin.y,
        spec.rect.size.width,
        spec.rect.size.height,
        background,
        border,
        // A pill rather than a rounded rectangle: at a glance that is what
        // separates a button from the text field sitting next to it.
        spec.rect.size.height / 2.0,
    )?;
    // Centre the label. It used to be pinned to the left inset, which reads as
    // a layout bug on any button wider than its text -- most obviously on the
    // equal-width buttons of a confirmation dialog.
    let available = button_label_width(spec.rect.size.width, ctx.scale());
    let measured = ctx.measure_text_width(font, spec.label).min(available);
    let text_x =
        spec.rect.origin.x + ((spec.rect.size.width - measured) / 2.0).max(ctx.px(BUTTON_TEXT_PAD));
    ctx.draw_text_on_layer(
        layers,
        text_layer,
        font,
        text_x,
        control_text_y(ctx, spec.rect.origin.y, spec.rect.size.height),
        spec.label,
        label_color,
        available,
    )?;
    Ok(())
}

/// A transparent square button containing a tinted SVG icon; background fades
/// in on hover/press.
pub(crate) fn draw_icon_button<A: Copy + PartialEq>(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    widgets: &mut UiContext<A>,
    interaction: &InteractionState<A>,
    palette: UiPalette,
    x: f32,
    y: f32,
    size: f32,
    icon: SvgIcon,
    action: A,
) -> anyhow::Result<()> {
    draw_icon_button_on_layer(
        ctx,
        layers,
        widgets,
        interaction,
        palette,
        x,
        y,
        size,
        icon,
        action,
        0,
    )
}

/// The standard icon button with its optional hover/press surface placed on a
/// caller-selected layer. This is useful for fixed chrome that must be redrawn
/// above a scrolling-content mask while retaining the shared sizing, padding,
/// colors and hit target.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_icon_button_on_layer<A: Copy + PartialEq>(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    widgets: &mut UiContext<A>,
    interaction: &InteractionState<A>,
    palette: UiPalette,
    x: f32,
    y: f32,
    size: f32,
    icon: SvgIcon,
    action: A,
    background_layer: usize,
) -> anyhow::Result<()> {
    let rect = RectF::new(euclid::point2(x, y), euclid::size2(size, size));
    widgets.push(rect, WidgetKind::Button, action);
    let hovered = interaction.hovered == Some(action);
    let pressed = interaction.pressed == Some(action);
    let bg = if pressed {
        palette.control_pressed_bg
    } else if hovered {
        palette.control_hover_bg
    } else {
        LinearRgba::TRANSPARENT
    };
    if bg.3 > 0.0 {
        ctx.draw_rounded_rect(
            layers,
            background_layer,
            x,
            y,
            size,
            size,
            bg,
            ctx.px(ICON_BUTTON_RADIUS),
        )?;
    }
    // cell_size already tracks the window DPI, so only the padding and the
    // clamp bounds need converting from design pixels.
    let icon_size = (ctx.metrics.cell_size.height as f32 + ctx.px(ICON_CELL_PADDING))
        .clamp(ctx.px(ICON_MIN_SIZE), ctx.px(ICON_MAX_SIZE));
    ctx.draw_svg_icon(
        layers,
        icon,
        x + (size - icon_size) / 2.0,
        y + (size - icon_size) / 2.0,
        icon_size,
        if hovered || pressed {
            palette.text
        } else {
            palette.muted_text
        },
    )
}

/// Caret and selection to render inside a text field, in char indices into the
/// field's text. `None` at the call site keeps the legacy behaviour: the caret
/// pins to the end of the text and only whole-text selection can be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct InputCaret {
    pub cursor: usize,
    /// Ordered `(start, end)` char range; empty ranges are treated as no
    /// selection.
    pub selection: Option<(usize, usize)>,
}

/// Width of `text[..char_idx]` as it will actually be drawn. Shares
/// [`DrawContext::measure_text_width`] with the renderer so caret placement can
/// never drift from the glyphs.
pub(crate) fn text_width_to_char(
    ctx: &DrawContext,
    font: &Rc<LoadedFont>,
    text: &str,
    char_idx: usize,
) -> f32 {
    let byte_idx = text
        .char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());
    ctx.measure_text_width(font, &text[..byte_idx])
}

/// Char index nearest to `dx` (an offset from the start of the text). Rounds to
/// the closer boundary so clicking the right half of a glyph lands after it,
/// which is what every native text field does.
pub(crate) fn char_index_for_x(
    ctx: &DrawContext,
    font: &Rc<LoadedFont>,
    text: &str,
    dx: f32,
) -> usize {
    if text.is_empty() || dx <= 0.0 {
        return 0;
    }
    let char_len = text.chars().count();
    // Binary search for the last boundary that still starts before `dx`, then
    // pick whichever of the two neighbouring boundaries is closer.
    let (mut low, mut high) = (0usize, char_len);
    while low < high {
        let mid = (low + high + 1) / 2;
        if text_width_to_char(ctx, font, text, mid) <= dx {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    if low >= char_len {
        return char_len;
    }
    let before = text_width_to_char(ctx, font, text, low);
    let after = text_width_to_char(ctx, font, text, low + 1);
    if dx - before > (after - before) / 2.0 {
        low + 1
    } else {
        low
    }
}

/// A single-line text field with placeholder, focus ring and caret. `text_pad`
/// is the horizontal inset for the text (left/right). Pass `caret` to render a
/// real caret position and partial selection; see [`InputCaret`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_text_input<A: Copy + PartialEq>(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    font: &Rc<LoadedFont>,
    widgets: &mut UiContext<A>,
    interaction: &InteractionState<A>,
    palette: UiPalette,
    tokens: UiTokens,
    text_pad: f32,
    cursor_on: bool,
    spec: TextInputSpec<'_, A>,
    caret: Option<InputCaret>,
) -> anyhow::Result<()> {
    draw_text_input_with_layers(
        ctx,
        layers,
        font,
        widgets,
        interaction,
        palette,
        tokens,
        text_pad,
        cursor_on,
        spec,
        caret,
        0,
        1,
    )
}

/// The same field drawn entirely on one layer. A field that sits in a fixed
/// header has scrolled content passing beneath it, and the mask that hides
/// that content covers every layer -- so the field has to be painted above
/// the mask, not merely after it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_text_input_on_layer<A: Copy + PartialEq>(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    font: &Rc<LoadedFont>,
    widgets: &mut UiContext<A>,
    interaction: &InteractionState<A>,
    palette: UiPalette,
    tokens: UiTokens,
    text_pad: f32,
    cursor_on: bool,
    spec: TextInputSpec<'_, A>,
    caret: Option<InputCaret>,
    layer_num: usize,
) -> anyhow::Result<()> {
    draw_text_input_with_layers(
        ctx,
        layers,
        font,
        widgets,
        interaction,
        palette,
        tokens,
        text_pad,
        cursor_on,
        spec,
        caret,
        layer_num,
        layer_num,
    )
}

#[allow(clippy::too_many_arguments)]
fn draw_text_input_with_layers<A: Copy + PartialEq>(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    font: &Rc<LoadedFont>,
    widgets: &mut UiContext<A>,
    interaction: &InteractionState<A>,
    palette: UiPalette,
    tokens: UiTokens,
    text_pad: f32,
    cursor_on: bool,
    spec: TextInputSpec<'_, A>,
    caret: Option<InputCaret>,
    frame_layer: usize,
    text_layer: usize,
) -> anyhow::Result<()> {
    widgets.push(spec.rect, WidgetKind::TextInput, spec.action);
    let border = if spec.focused {
        palette.accent
    } else if interaction.hovered == Some(spec.action) {
        palette.separator
    } else {
        palette.control_border
    };
    ctx.draw_rounded_frame(
        layers,
        frame_layer,
        spec.rect.origin.x,
        spec.rect.origin.y,
        spec.rect.size.width,
        spec.rect.size.height,
        palette.control_bg,
        border,
        tokens.control_radius,
    )?;

    // An empty field keeps its placeholder while focused. Hiding it left a
    // blank slab with a caret in it, which says nothing about what belongs
    // there -- and the field this page focuses on open is the search box.
    let text = if spec.text.is_empty() {
        spec.placeholder
    } else {
        spec.text
    };
    let color = if spec.text.is_empty() {
        palette.muted_text
    } else {
        palette.text
    };
    let text_left = spec.rect.origin.x + text_pad;
    let text_area = (spec.rect.size.width - text_pad * 2.0).max(0.0);
    let bleed_x = ctx.px(INPUT_SELECTION_BLEED_X);
    let inset_y = ctx.px(INPUT_SELECTION_INSET_Y);
    let highlight_y = spec.rect.origin.y + inset_y;
    let highlight_h = (spec.rect.size.height - inset_y * 2.0).max(0.0);
    let highlight_radius = (tokens.control_radius - ctx.px(INPUT_SELECTION_RADIUS_INSET))
        .max(ctx.px(INPUT_SELECTION_MIN_RADIUS));

    // Explicit range wins; `selected_all` is the legacy whole-text fallback.
    let selection = caret
        .and_then(|caret| caret.selection)
        .filter(|(start, end)| start != end)
        .or_else(|| {
            (spec.selected_all && !spec.text.is_empty()).then(|| (0, spec.text.chars().count()))
        });

    if spec.focused && !spec.text.is_empty() {
        if let Some((start, end)) = selection {
            let start_x = text_width_to_char(ctx, font, spec.text, start).min(text_area);
            let end_x = text_width_to_char(ctx, font, spec.text, end).min(text_area);
            ctx.draw_rounded_rect(
                layers,
                text_layer,
                text_left + start_x - bleed_x,
                highlight_y,
                (end_x - start_x) + bleed_x * 2.0,
                highlight_h,
                palette.accent.mul_alpha(0.56),
                highlight_radius,
            )?;
        }
    }
    ctx.draw_text_on_layer(
        layers,
        text_layer,
        font,
        text_left,
        control_text_y(ctx, spec.rect.origin.y, spec.rect.size.height),
        text,
        color,
        text_area,
    )?;
    if spec.focused && selection.is_none() && cursor_on {
        // Without an explicit caret the field can only pin to the end of the
        // text, which is the pre-caret behaviour.
        let caret_dx = match caret {
            Some(caret) => text_width_to_char(ctx, font, spec.text, caret.cursor),
            None => ctx.measure_text_width(font, spec.text),
        };
        let caret_width = ctx.px(CARET_WIDTH);
        ctx.draw_rect(
            layers,
            text_layer,
            text_left + caret_dx.min(text_area) - caret_width / 3.0,
            highlight_y,
            caret_width,
            highlight_h,
            palette.accent,
        )?;
    }
    Ok(())
}

/// A pill switch. The on track is the accent; the off track is a filled
/// neutral, not `control_border` -- a hairline colour used as a fill left the
/// two states nearly identical in the dark palette.
pub(crate) fn draw_toggle<A: Copy + PartialEq>(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    widgets: &mut UiContext<A>,
    interaction: &InteractionState<A>,
    palette: UiPalette,
    rect: RectF,
    on: bool,
    action: A,
) -> anyhow::Result<()> {
    widgets.push(rect, WidgetKind::Button, action);
    let h = rect.size.height;
    let pressed = interaction.pressed == Some(action);
    let hovered = interaction.hovered == Some(action);
    let track = toggle_track_color(palette, on, hovered, pressed);
    ctx.draw_rounded_rect(
        layers,
        0,
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        h,
        track,
        h / 2.0,
    )?;
    let inset = ctx.px(TOGGLE_KNOB_INSET);
    let knob = (h - inset * 2.0).max(inset);
    let knob_x = if on {
        rect.origin.x + rect.size.width - knob - inset
    } else {
        rect.origin.x + inset
    };
    ctx.draw_rounded_rect(
        layers,
        1,
        knob_x,
        rect.origin.y + inset,
        knob,
        knob,
        palette.on_accent,
        knob / 2.0,
    )?;
    Ok(())
}

/// The track colour of a switch in every state. The settings window paints
/// its own switch -- it has no `DrawContext`, so it cannot call
/// [`draw_toggle`] -- and this is the part the two must agree on, so it lives
/// here rather than being written twice.
pub(crate) fn toggle_track_color(
    palette: UiPalette,
    on: bool,
    hovered: bool,
    pressed: bool,
) -> LinearRgba {
    if on {
        if pressed || hovered {
            palette.accent_hover
        } else {
            palette.accent
        }
    } else if pressed {
        palette.track_off.mul_alpha(0.8)
    } else if hovered {
        palette.track_off.mul_alpha(0.9)
    } else {
        palette.track_off
    }
}

/// A vertical scrollbar thumb within `area` (no track), matching the settings
/// window's look.
pub(crate) fn draw_scrollbar(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    palette: UiPalette,
    tokens: UiTokens,
    area: RectF,
    scroll: ScrollState,
) -> anyhow::Result<()> {
    draw_scrollbar_on_layer(ctx, layers, palette, tokens, area, scroll, 0)
}

/// Draw the shared scrollbar thumb on a caller-selected layer. Scrollable
/// full-window views use this after their viewport masks so the thumb remains
/// crisp at the outer window edge.
pub(crate) fn draw_scrollbar_on_layer(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    palette: UiPalette,
    tokens: UiTokens,
    area: RectF,
    scroll: ScrollState,
    layer_num: usize,
) -> anyhow::Result<()> {
    let spec = ScrollbarSpec::from_area(area, tokens);
    if let Some((thumb_y, thumb_h)) =
        scroll.thumb_with_min(spec.y, spec.height, tokens.scrollbar_min_thumb)
    {
        ctx.draw_rounded_rect(
            layers,
            layer_num,
            spec.x,
            thumb_y,
            spec.width,
            thumb_h,
            palette.scrollbar_thumb,
            spec.width / 2.0,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{button_label_width, BUTTON_TEXT_PAD};

    /// Callers size buttons as `measure_text_width(label) + px(56)`. The label
    /// must still fit after the widget subtracts its own insets, at *every*
    /// scale — mixing a scaled width with unscaled insets is what truncated
    /// "Save & Open" into "Save & O..." on 96dpi displays.
    #[test]
    fn label_always_fits_the_button_the_caller_sized() {
        const CALLER_PAD: f32 = 56.0;
        for scale in [0.25, 0.5, 0.75, 1.0, 2.0] {
            for measured in [0.0, 12.5, 61.0, 168.0] {
                let button_width = measured + CALLER_PAD * scale;
                let available = button_label_width(button_width, scale);
                assert!(
                    available >= measured,
                    "scale {}: {} < {}",
                    scale,
                    available,
                    measured
                );
            }
        }
    }

    #[test]
    fn insets_are_symmetric() {
        // The inset comes off both sides and scales with the button, which
        // is what leaves the house size its 16 design px of slack.
        assert_eq!(
            button_label_width(100.0, 1.0),
            100.0 - BUTTON_TEXT_PAD * 2.0
        );
        assert_eq!(button_label_width(100.0, 0.5), 100.0 - BUTTON_TEXT_PAD);
    }
}
