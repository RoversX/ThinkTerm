pub(crate) use crate::termwindow::ui::icons::SvgIcon;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SettingsIcon {
    General,
    Appearance,
    Terminal,
    Workspaces,
    Keymap,
    Sync,
    Developer,
    UiKit,
    Memory,
    About,
    Search,
    Clear,
    External,
}

impl SettingsIcon {
    pub(crate) fn svg(self) -> SvgIcon {
        match self {
            Self::General => SvgIcon::SlidersHorizontal,
            Self::Appearance => SvgIcon::Palette,
            Self::Terminal => SvgIcon::Terminal,
            Self::Workspaces => SvgIcon::FolderOpen,
            Self::Keymap => SvgIcon::Keyboard,
            Self::Sync => SvgIcon::Cloud,
            Self::Developer => SvgIcon::Settings,
            Self::UiKit => SvgIcon::SlidersVertical,
            Self::Memory => SvgIcon::MemoryStick,
            Self::About => SvgIcon::Info,
            Self::Search => SvgIcon::Search,
            Self::Clear => SvgIcon::X,
            Self::External => SvgIcon::ExternalLink,
        }
    }
}
