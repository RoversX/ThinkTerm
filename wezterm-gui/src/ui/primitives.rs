use crate::ui::{UiPalette, UiTokens, WidgetKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlState {
    Normal,
    Hovered,
    Pressed,
    Active,
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
            Self::Pressed => (palette.control_pressed_bg, palette.selected_bg),
            Self::Active => (palette.selected_bg, palette.selected_bg),
            Self::Disabled => (palette.control_bg, palette.control_border),
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
