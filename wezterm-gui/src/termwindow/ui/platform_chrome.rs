use crate::termwindow::ui::tokens::{
    MACOS_TITLEBAR_CONTENT_TOP_INSET, MACOS_TRAFFIC_LIGHT_CLEARANCE_WIDTH,
    MACOS_WINDOW_TAB_RESERVED_ACTION_SLOTS, SIDEBAR_INSET,
    WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE, WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X,
    WINDOW_TAB_FULLSCREEN_SIDEBAR_ICON_SIZE, WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE,
    WINDOW_TAB_LEADING_ACTION_GAP, WINDOW_TAB_LEADING_ACTION_ICON_SIZE,
};
use crate::ui::{scale_ui_f32, scale_ui_usize};
use window::{
    IntegratedTitleButtonAlignment, IntegratedTitleButtonStyle, WindowDecorations, WindowState,
};

#[derive(Debug, Clone, Copy)]
pub struct WindowTabChromeParams {
    pub use_fancy_tab_bar: bool,
    pub workspace_sidebar_width: usize,
    pub window_state: WindowState,
    pub window_decorations: WindowDecorations,
    pub integrated_title_button_alignment: IntegratedTitleButtonAlignment,
    pub integrated_title_button_style: IntegratedTitleButtonStyle,
    pub cell_width: f32,
    pub dpi: usize,
}

pub fn uses_integrated_window_buttons(
    decorations: WindowDecorations,
    window_state: WindowState,
) -> bool {
    decorations.contains(WindowDecorations::INTEGRATED_BUTTONS)
        && !window_state.contains(WindowState::SERVER_DECORATED)
}

impl WindowTabChromeParams {
    fn px(self, value: usize) -> usize {
        scale_ui_usize(value, self.dpi)
    }

    fn px_f32(self, value: f32) -> f32 {
        scale_ui_f32(value, self.dpi)
    }

    pub fn leading_action_slot_count(self) -> usize {
        if !self.use_fancy_tab_bar || self.workspace_sidebar_width > 0 {
            return 0;
        }

        if self.shows_sidebar_toggle_action() {
            return 1;
        }

        if cfg!(target_os = "macos") {
            return MACOS_WINDOW_TAB_RESERVED_ACTION_SLOTS;
        }

        0
    }

    pub fn uses_integrated_window_buttons(self) -> bool {
        uses_integrated_window_buttons(self.window_decorations, self.window_state)
    }

    pub fn shows_sidebar_toggle_action(self) -> bool {
        self.use_fancy_tab_bar
            && self.workspace_sidebar_width == 0
            && (self.window_state.contains(WindowState::FULL_SCREEN) || !cfg!(target_os = "macos"))
    }

    pub fn leading_action_start_pixels(self) -> f32 {
        if self.leading_action_slot_count() == 0 {
            0.0
        } else if self.window_state.contains(WindowState::FULL_SCREEN) {
            self.px(WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X) as f32
        } else if cfg!(target_os = "macos") {
            self.px(MACOS_TRAFFIC_LIGHT_CLEARANCE_WIDTH) as f32
        } else {
            self.px(SIDEBAR_INSET) as f32
        }
    }

    pub fn leading_action_area_width_pixels(self) -> f32 {
        let count = self.leading_action_slot_count();
        if count == 0 {
            return 0.0;
        }

        let button_size = if self.shows_sidebar_toggle_action() {
            self.sidebar_toggle_button_size()
        } else {
            self.px(WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE)
        };
        (count * (button_size + self.px(WINDOW_TAB_LEADING_ACTION_GAP))) as f32
    }

    pub fn sidebar_toggle_uses_fullscreen_style(self) -> bool {
        self.window_state.contains(WindowState::FULL_SCREEN)
    }

    pub fn sidebar_toggle_button_size(self) -> usize {
        self.px(workspace_sidebar_toolbar_button_size(self.window_state))
    }

    pub fn sidebar_toggle_icon_size(self) -> usize {
        self.px(workspace_sidebar_toolbar_icon_size(self.window_state))
    }

    pub fn left_padding_pixels(self) -> f32 {
        if self.workspace_sidebar_width > 0 {
            return 0.0;
        }

        let leading_action_padding = if self.leading_action_slot_count() > 0 {
            self.leading_action_start_pixels() + self.leading_action_area_width_pixels()
        } else {
            0.0
        };

        if cfg!(target_os = "macos") && !self.window_state.contains(WindowState::FULL_SCREEN) {
            return leading_action_padding.max(self.px(MACOS_TRAFFIC_LIGHT_CLEARANCE_WIDTH) as f32);
        }

        if self.uses_integrated_window_buttons()
            && (self.integrated_title_button_alignment == IntegratedTitleButtonAlignment::Left
                || self.integrated_title_button_style == IntegratedTitleButtonStyle::MacOsNative)
        {
            if self.integrated_title_button_style == IntegratedTitleButtonStyle::MacOsNative {
                if self.window_state.contains(WindowState::FULL_SCREEN) {
                    leading_action_padding + self.cell_width * 0.5
                } else {
                    self.px_f32(70.0)
                }
            } else {
                leading_action_padding
            }
        } else {
            leading_action_padding + self.cell_width * 0.5
        }
    }
}

pub fn workspace_sidebar_content_top(
    panel_y: usize,
    sidebar_inset: usize,
    tab_row_height: usize,
    window_state: WindowState,
    dpi: usize,
) -> usize {
    if cfg!(target_os = "macos") && !window_state.contains(WindowState::FULL_SCREEN) {
        panel_y + scale_ui_usize(MACOS_TITLEBAR_CONTENT_TOP_INSET, dpi).max(tab_row_height)
    } else {
        panel_y + sidebar_inset
    }
}

pub fn workspace_sidebar_shows_toolbar(window_state: WindowState) -> bool {
    window_state.contains(WindowState::FULL_SCREEN) || !cfg!(target_os = "macos")
}

pub fn workspace_sidebar_toolbar_uses_fullscreen_style(window_state: WindowState) -> bool {
    window_state.contains(WindowState::FULL_SCREEN)
}

/// Sizes are design pixels (2x macOS backing); callers scale via ui_px.
pub fn workspace_sidebar_toolbar_button_size(window_state: WindowState) -> usize {
    if workspace_sidebar_toolbar_uses_fullscreen_style(window_state) {
        WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE
    } else {
        WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE
    }
}

pub fn workspace_sidebar_toolbar_icon_size(window_state: WindowState) -> usize {
    if workspace_sidebar_toolbar_uses_fullscreen_style(window_state) {
        WINDOW_TAB_FULLSCREEN_SIDEBAR_ICON_SIZE
    } else {
        WINDOW_TAB_LEADING_ACTION_ICON_SIZE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_decorations_suppress_integrated_window_buttons() {
        let decorations = WindowDecorations::INTEGRATED_BUTTONS | WindowDecorations::RESIZE;
        assert!(uses_integrated_window_buttons(
            decorations,
            WindowState::default()
        ));
        assert!(!uses_integrated_window_buttons(
            decorations,
            WindowState::SERVER_DECORATED
        ));
    }

    #[test]
    fn toolbar_sizes_are_shared_design_pixels() {
        // Same design-pixel values on every platform; ui_px maps them to
        // the local scale (halving at 96dpi), so per-OS values would
        // double-apply the reduction.
        assert_eq!(
            workspace_sidebar_toolbar_button_size(WindowState::default()),
            WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE
        );
        assert_eq!(
            workspace_sidebar_toolbar_icon_size(WindowState::default()),
            WINDOW_TAB_LEADING_ACTION_ICON_SIZE
        );
        assert_eq!(
            workspace_sidebar_toolbar_button_size(WindowState::FULL_SCREEN),
            WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE
        );
        assert_eq!(
            workspace_sidebar_toolbar_icon_size(WindowState::FULL_SCREEN),
            WINDOW_TAB_FULLSCREEN_SIDEBAR_ICON_SIZE
        );
    }
}
