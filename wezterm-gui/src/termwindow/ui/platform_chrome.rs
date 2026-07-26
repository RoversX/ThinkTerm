use crate::termwindow::ui::tokens::{
    MACOS_TITLEBAR_CONTENT_TOP_INSET, MACOS_TRAFFIC_LIGHT_CLEARANCE_WIDTH,
    MACOS_WINDOW_TAB_RESERVED_ACTION_SLOTS, SIDEBAR_INSET, TAB_VERTICAL_PADDING,
    WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE, WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X,
    WINDOW_TAB_FULLSCREEN_SIDEBAR_ICON_SIZE, WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE,
    WINDOW_TAB_LEADING_ACTION_GAP, WINDOW_TAB_LEADING_ACTION_ICON_SIZE, WINDOW_TAB_TOP_SPACER,
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
    /// Full fancy tab-bar row height when one is visible at the TOP of
    /// the window; None for hidden, retro or bottom tab bars. Drives the
    /// sidebar-toggle size so layout reservation matches painting.
    pub top_fancy_row_height: Option<usize>,
}

/// Single source of truth for the workspace-sidebar toggle size: the
/// collapsed (tab bar) painter, the expanded (sidebar) painter and the
/// tab layout reservation all consume this so they can never disagree.
/// With a visible top fancy bar the toggle matches the window tab capsule
/// height (full row minus top spacer minus vertical padding), floored at
/// the fixed toolbar size so tiny fonts never make it unusable.
pub fn sidebar_toggle_size_px(
    dpi: usize,
    window_state: WindowState,
    top_fancy_row_height: Option<usize>,
) -> usize {
    let px = |v: usize| scale_ui_usize(v, dpi);
    if window_state.contains(WindowState::FULL_SCREEN) {
        return px(WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE);
    }
    if cfg!(target_os = "macos") {
        return px(WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE);
    }
    if let Some(row_h) = top_fancy_row_height {
        if row_h > 0 {
            let spacer = px(WINDOW_TAB_TOP_SPACER).min(row_h);
            let capsule = (row_h - spacer).saturating_sub(px(TAB_VERTICAL_PADDING) * 2);
            return capsule.max(px(WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE));
        }
    }
    px(WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE)
}

/// Distance from the window content's left edge to the sidebar toggle.
/// Shared by the collapsed and expanded states and deliberately
/// independent of the current sidebar width, so opening or closing the
/// sidebar never moves the control.
pub fn sidebar_toggle_left_inset_px(dpi: usize, window_state: WindowState) -> usize {
    let px = |v: usize| scale_ui_usize(v, dpi);
    if window_state.contains(WindowState::FULL_SCREEN) {
        px(WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_X)
    } else if cfg!(target_os = "macos") {
        px(MACOS_TRAFFIC_LIGHT_CLEARANCE_WIDTH)
    } else {
        // The extra inset clears the rounded window corner.
        px(SIDEBAR_INSET) + px(12)
    }
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
        } else {
            sidebar_toggle_left_inset_px(self.dpi, self.window_state) as f32
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
        sidebar_toggle_size_px(self.dpi, self.window_state, self.top_fancy_row_height)
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

    fn test_params(
        workspace_sidebar_width: usize,
        window_state: WindowState,
        top_fancy_row_height: Option<usize>,
    ) -> WindowTabChromeParams {
        WindowTabChromeParams {
            use_fancy_tab_bar: true,
            workspace_sidebar_width,
            window_state,
            window_decorations: WindowDecorations::INTEGRATED_BUTTONS | WindowDecorations::RESIZE,
            integrated_title_button_alignment: IntegratedTitleButtonAlignment::Right,
            integrated_title_button_style: IntegratedTitleButtonStyle::Windows,
            cell_width: 10.0,
            dpi: 96,
            top_fancy_row_height,
        }
    }

    #[test]
    fn sidebar_toggle_size_tracks_the_fancy_row_and_never_collapses() {
        if cfg!(target_os = "macos") {
            return;
        }
        let px = |v: usize| scale_ui_usize(v, 96);
        let fixed = px(WINDOW_TAB_LEADING_ACTION_BUTTON_SIZE);
        // Capsule = row - spacer - 2 * vertical padding
        let capsule = |row: usize| row - px(WINDOW_TAB_TOP_SPACER) - px(TAB_VERTICAL_PADDING) * 2;
        // Normal and large tab fonts follow the row height
        assert_eq!(
            sidebar_toggle_size_px(96, WindowState::default(), Some(38)),
            capsule(38)
        );
        assert_eq!(
            sidebar_toggle_size_px(96, WindowState::default(), Some(58)),
            capsule(58)
        );
        // Hidden, retro or bottom tab bars keep the fixed usable size
        assert_eq!(
            sidebar_toggle_size_px(96, WindowState::default(), None),
            fixed
        );
        // Tiny rows never shrink the control below the fixed size
        assert_eq!(
            sidebar_toggle_size_px(96, WindowState::default(), Some(8)),
            fixed
        );
        // Fullscreen style has its own size on every platform
        assert_eq!(
            sidebar_toggle_size_px(96, WindowState::FULL_SCREEN, Some(38)),
            px(WINDOW_TAB_FULLSCREEN_SIDEBAR_BUTTON_SIZE)
        );
    }

    #[test]
    fn toggle_reservation_matches_painted_size() {
        if cfg!(target_os = "macos") {
            return;
        }
        // A large tab font grows the painted toggle; the tab layout must
        // reserve at least that much so the first tab cannot overlap it.
        for row in [38, 58, 90] {
            let params = test_params(0, WindowState::default(), Some(row));
            let painted = params.sidebar_toggle_button_size();
            assert_eq!(
                painted,
                sidebar_toggle_size_px(96, WindowState::default(), Some(row))
            );
            assert!(
                params.leading_action_area_width_pixels() >= painted as f32,
                "row {row}: reserved {} < painted {painted}",
                params.leading_action_area_width_pixels()
            );
        }
    }

    #[test]
    fn toggle_inset_is_independent_of_sidebar_width() {
        if cfg!(target_os = "macos") {
            return;
        }
        // The expanded painter anchors on sidebar_toggle_left_inset_px;
        // the collapsed tab-bar path uses leading_action_start_pixels.
        // They must agree whenever the tab-bar path is active.
        let inset = sidebar_toggle_left_inset_px(96, WindowState::default());
        let collapsed = test_params(0, WindowState::default(), Some(38));
        assert_eq!(collapsed.leading_action_start_pixels(), inset as f32);
        // Fullscreen too
        let fs_inset = sidebar_toggle_left_inset_px(96, WindowState::FULL_SCREEN);
        let fs = test_params(0, WindowState::FULL_SCREEN, Some(38));
        assert_eq!(fs.leading_action_start_pixels(), fs_inset as f32);
        // And the size is identical whether the sidebar is open or closed
        let open = test_params(380, WindowState::default(), Some(38));
        assert_eq!(
            open.sidebar_toggle_button_size(),
            collapsed.sidebar_toggle_button_size()
        );
    }

    #[test]
    fn toolbar_sizes_are_shared_design_pixels() {
        // Same design-pixel values on every platform; ui_px maps them to
        // the local scale (halving at 96dpi), so per-OS values would
        // double-apply the reduction. (Button sizing moved to the shared
        // sidebar_toggle_size_px geometry, covered by the tests above.)
        assert_eq!(
            workspace_sidebar_toolbar_icon_size(WindowState::default()),
            WINDOW_TAB_LEADING_ACTION_ICON_SIZE
        );
        assert_eq!(
            workspace_sidebar_toolbar_icon_size(WindowState::FULL_SCREEN),
            WINDOW_TAB_FULLSCREEN_SIDEBAR_ICON_SIZE
        );
    }
}
