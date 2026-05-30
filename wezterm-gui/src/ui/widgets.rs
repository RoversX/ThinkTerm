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

/// Vertically center single-line text of the UI font within `height`.
fn control_text_y(ctx: &DrawContext, y: f32, height: f32) -> f32 {
    let cell_height = ctx.metrics.cell_size.height as f32;
    y + ((height - cell_height) / 2.0).max(0.0)
}

/// A filled, rounded button with a centered-left label. The caller fills
/// `spec.state` (e.g. from its [`InteractionState`]).
pub(crate) fn draw_button<A: Copy + PartialEq>(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    font: &Rc<LoadedFont>,
    widgets: &mut UiContext<A>,
    palette: UiPalette,
    spec: ButtonSpec<A>,
) -> anyhow::Result<()> {
    widgets.push(spec.rect, spec.kind, spec.action);
    let (background, border) = spec.state.colors(palette);
    ctx.draw_rounded_frame(
        layers,
        0,
        spec.rect.origin.x,
        spec.rect.origin.y,
        spec.rect.size.width,
        spec.rect.size.height,
        background,
        border,
        8.0,
    )?;
    ctx.draw_text(
        layers,
        font,
        spec.rect.origin.x + 14.0,
        control_text_y(ctx, spec.rect.origin.y, spec.rect.size.height),
        spec.label,
        palette.text,
        spec.rect.size.width - 28.0,
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
        ctx.draw_rounded_rect(layers, 0, x, y, size, size, bg, 8.0)?;
    }
    let icon_size = (ctx.metrics.cell_size.height as f32 + 4.0).clamp(20.0, 30.0);
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

/// A single-line text field with placeholder, focus ring and caret. `text_pad`
/// is the horizontal inset for the text (left/right).
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
) -> anyhow::Result<()> {
    widgets.push(spec.rect, WidgetKind::TextInput, spec.action);
    let border = if spec.focused {
        palette.selected_bg
    } else if interaction.hovered == Some(spec.action) {
        palette.separator
    } else {
        palette.control_border
    };
    ctx.draw_rounded_frame(
        layers,
        0,
        spec.rect.origin.x,
        spec.rect.origin.y,
        spec.rect.size.width,
        spec.rect.size.height,
        palette.control_bg,
        border,
        tokens.control_radius,
    )?;

    let text = if spec.text.is_empty() && !spec.focused {
        spec.placeholder
    } else {
        spec.text
    };
    let color = if spec.text.is_empty() && !spec.focused {
        palette.muted_text
    } else {
        palette.text
    };
    if spec.focused && spec.selected_all && !spec.text.is_empty() {
        let selection_width = ctx
            .measure_text_width(font, spec.text)
            .min((spec.rect.size.width - text_pad * 2.0).max(0.0));
        ctx.draw_rounded_rect(
            layers,
            1,
            spec.rect.origin.x + text_pad - 4.0,
            spec.rect.origin.y + 5.0,
            selection_width + 8.0,
            spec.rect.size.height - 10.0,
            palette.selected_bg.mul_alpha(0.56),
            (tokens.control_radius - 5.0).max(3.0),
        )?;
    }
    ctx.draw_text(
        layers,
        font,
        spec.rect.origin.x + text_pad,
        control_text_y(ctx, spec.rect.origin.y, spec.rect.size.height),
        text,
        color,
        (spec.rect.size.width - text_pad * 2.0).max(0.0),
    )?;
    if spec.focused && !spec.selected_all && cursor_on {
        let caret_x = spec.rect.origin.x
            + text_pad
            + ctx
                .measure_text_width(font, spec.text)
                .min((spec.rect.size.width - text_pad * 2.0).max(0.0));
        ctx.draw_rect(
            layers,
            1,
            caret_x - 1.0,
            spec.rect.origin.y + 5.0,
            3.0,
            spec.rect.size.height - 10.0,
            palette.selected_bg,
        )?;
    }
    Ok(())
}

/// A pill toggle (iOS-style). `on` selects accent vs muted track + knob side.
pub(crate) fn draw_toggle<A: Copy + PartialEq>(
    ctx: &DrawContext,
    layers: &mut TripleLayerQuadAllocator<'_>,
    widgets: &mut UiContext<A>,
    palette: UiPalette,
    rect: RectF,
    on: bool,
    action: A,
) -> anyhow::Result<()> {
    widgets.push(rect, WidgetKind::Button, action);
    let h = rect.size.height;
    let track = if on {
        palette.selected_bg
    } else {
        palette.control_border
    };
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
    let knob = (h - 4.0).max(2.0);
    let knob_x = if on {
        rect.origin.x + rect.size.width - knob - 2.0
    } else {
        rect.origin.x + 2.0
    };
    ctx.draw_rounded_rect(
        layers,
        1,
        knob_x,
        rect.origin.y + 2.0,
        knob,
        knob,
        LinearRgba::with_components(1.0, 1.0, 1.0, 1.0),
        knob / 2.0,
    )?;
    Ok(())
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
    let spec = ScrollbarSpec::from_area(area, tokens);
    if let Some((thumb_y, thumb_h)) = scroll.thumb(spec.y, spec.height) {
        ctx.draw_rounded_rect(
            layers,
            0,
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
