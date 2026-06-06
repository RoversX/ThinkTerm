use crate::termwindow::ui::tokens::{
    MACOS_TITLEBAR_CONTENT_TOP_INSET, MACOS_TRAFFIC_LIGHT_CLEARANCE_WIDTH,
    MACOS_WINDOW_TAB_RESERVED_ACTION_SLOTS, SIDEBAR_INSET,
    WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE, WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X,
    WINDOW_TAB_FULLSCREEN_SIDEBAR_ICON_SIZE, WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE,
    WINDOW_TAB_LEADING_ACTION_GAP, WINDOW_TAB_LEADING_ACTION_ICON_SIZE,
};
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
}

impl WindowTabChromeParams {
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

    pub fn shows_sidebar_toggle_action(self) -> bool {
        self.use_fancy_tab_bar
            && self.workspace_sidebar_width == 0
            && (self.window_state.contains(WindowState::FULL_SCREEN) || !cfg!(target_os = "macos"))
    }

    pub fn leading_action_start_pixels(self) -> f32 {
        if self.leading_action_slot_count() == 0 {
            0.0
        } else if self.window_state.contains(WindowState::FULL_SCREEN) {
            WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X as f32
        } else if cfg!(target_os = "macos") {
            MACOS_TRAFFIC_LIGHT_CLEARANCE_WIDTH as f32
        } else {
            SIDEBAR_INSET as f32
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
            WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE
        };
        (count * (button_size + WINDOW_TAB_LEADING_ACTION_GAP)) as f32
    }

    pub fn sidebar_toggle_uses_fullscreen_style(self) -> bool {
        self.window_state.contains(WindowState::FULL_SCREEN)
    }

    pub fn sidebar_toggle_button_size(self) -> usize {
        workspace_sidebar_toolbar_button_size(self.window_state)
    }

    pub fn sidebar_toggle_icon_size(self) -> usize {
        workspace_sidebar_toolbar_icon_size(self.window_state)
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
            return leading_action_padding.max(MACOS_TRAFFIC_LIGHT_CLEARANCE_WIDTH as f32);
        }

        if self
            .window_decorations
            .contains(WindowDecorations::INTEGRATED_BUTTONS)
            && (self.integrated_title_button_alignment == IntegratedTitleButtonAlignment::Left
                || self.integrated_title_button_style == IntegratedTitleButtonStyle::MacOsNative)
        {
            if self.integrated_title_button_style == IntegratedTitleButtonStyle::MacOsNative {
                if self.window_state.contains(WindowState::FULL_SCREEN) {
                    leading_action_padding + self.cell_width * 0.5
                } else {
                    70.0
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
) -> usize {
    if cfg!(target_os = "macos") && !window_state.contains(WindowState::FULL_SCREEN) {
        panel_y + MACOS_TITLEBAR_CONTENT_TOP_INSET.max(tab_row_height)
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

pub fn workspace_sidebar_toolbar_button_size(window_state: WindowState) -> usize {
    if workspace_sidebar_toolbar_uses_fullscreen_style(window_state) {
        WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE
    } else if !cfg!(target_os = "macos") {
        WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE + 8
    } else {
        WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE
    }
}

pub fn workspace_sidebar_toolbar_icon_size(window_state: WindowState) -> usize {
    if workspace_sidebar_toolbar_uses_fullscreen_style(window_state) {
        WINDOW_TAB_FULLSCREEN_SIDEBAR_ICON_SIZE
    } else if !cfg!(target_os = "macos") {
        WINDOW_TAB_LEADING_ACTION_ICON_SIZE + 6
    } else {
        WINDOW_TAB_LEADING_ACTION_ICON_SIZE
    }
}
