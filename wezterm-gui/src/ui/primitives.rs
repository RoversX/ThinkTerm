use crate::ui::{UiPalette, UiTokens, WidgetKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlState {
    Normal,
    Hovered,
    Pressed,
    Disabled,
}

impl ControlState {
    pub(crate) fn colors(
        self,
        palette: UiPalette,
    ) -> (window::color::LinearRgba, window::color::LinearRgba) {
        match self {
            Self::Normal => (palette.control_bg, palette.control_border),
            Self::Hovered => (palette.control_hover_bg, palette.separator),
            Self::Pressed => (palette.control_pressed_bg, palette.accent),
            Self::Disabled => (palette.control_bg, palette.control_border),
        }
    }
}

/// What a button *means*, which is what decides its colours. Kept apart from
/// [`ControlState`] because the two are orthogonal: a primary button still has
/// a hover and a pressed look.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ButtonVariant {
    /// Neutral chrome. Anything that is not the one action a panel exists for.
    #[default]
    Secondary,
    /// The one action a panel exists for: filled with the accent.
    Primary,
}

impl ButtonVariant {
    /// `(background, border, label)`. The label colour is the part the shared
    /// widget used to get wrong: it painted every label in `palette.text`, so
    /// a filled button got near-black text on saturated blue.
    pub(crate) fn colors(
        self,
        state: ControlState,
        palette: UiPalette,
    ) -> (
        window::color::LinearRgba,
        window::color::LinearRgba,
        window::color::LinearRgba,
    ) {
        let label_for_neutral = match state {
            ControlState::Disabled => palette.muted_text,
            _ => palette.text,
        };
        match self {
            Self::Secondary => {
                let (bg, border) = state.colors(palette);
                (bg, border, label_for_neutral)
            }
            Self::Primary => {
                let fill = match state {
                    // Both tints move the accent the same direction the
                    // appearance expects: darker in light mode, lighter in
                    // dark. Pressed and hovered share it -- on a filled
                    // button the extra step is not legible anyway.
                    ControlState::Hovered | ControlState::Pressed => palette.accent_hover,
                    ControlState::Disabled => palette.accent.mul_alpha(0.45),
                    _ => palette.accent,
                };
                let label = match state {
                    ControlState::Disabled => palette.on_accent.mul_alpha(0.65),
                    _ => palette.on_accent,
                };
                (fill, fill, label)
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ButtonSpec<'a, A: Copy> {
    pub label: &'a str,
    pub action: A,
    pub rect: window::RectF,
    pub state: ControlState,
    pub kind: WidgetKind,
    pub variant: ButtonVariant,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TextInputSpec<'a, A: Copy> {
    pub placeholder: &'a str,
    pub text: &'a str,
    pub rect: window::RectF,
    pub focused: bool,
    pub selected_all: bool,
    pub action: A,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SettingRowSpec<'a> {
    pub label: &'a str,
    pub description: &'a str,
    pub value: &'a str,
    pub y: f32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ScrollbarSpec {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl ScrollbarSpec {
    /// Every measurement here comes from `tokens`, which is already scaled for
    /// the target DPI (`UiTokens::for_dpi`). Do not reintroduce raw literals:
    /// mixing them with the scaled width breaks non-1.0 displays.
    pub(crate) fn from_area(area: window::RectF, tokens: UiTokens) -> Self {
        Self {
            x: area.origin.x + area.size.width - tokens.scrollbar_width - tokens.scrollbar_inset,
            y: area.origin.y + tokens.scrollbar_margin_y,
            width: tokens.scrollbar_width,
            height: (area.size.height - tokens.scrollbar_margin_y * 2.0).max(0.0),
        }
    }
}
