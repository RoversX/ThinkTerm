pub(crate) use crate::termwindow::ui::icons::{BrandIcon, SvgIcon};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SettingsIcon {
    General,
    Appearance,
    Sidebar,
    Terminal,
    Workspaces,
    Agents,
    Keymap,
    Sync,
    Developer,
    UiKit,
    Memory,
    Update,
    About,
    Archived,
    Web,
    CommandPalette,
    Search,
    Clear,
    External,
}

impl SettingsIcon {
    pub(crate) fn svg(self) -> SvgIcon {
        match self {
            Self::General => SvgIcon::SlidersHorizontal,
            Self::Appearance => SvgIcon::Palette,
            Self::Sidebar => SvgIcon::PanelRightOpen,
            Self::Terminal => SvgIcon::Terminal,
            Self::Workspaces => SvgIcon::FolderOpen,
            Self::Agents => SvgIcon::Bot,
            Self::Keymap => SvgIcon::Keyboard,
            Self::Sync => SvgIcon::Cloud,
            Self::Developer => SvgIcon::Settings,
            Self::UiKit => SvgIcon::SlidersVertical,
            Self::Memory => SvgIcon::MemoryStick,
            Self::Update => SvgIcon::RefreshCw,
            Self::About => SvgIcon::Info,
            Self::Archived => SvgIcon::Archive,
            Self::Web => SvgIcon::Globe,
            Self::CommandPalette => SvgIcon::SquareTerminal,
            Self::Search => SvgIcon::Search,
            Self::Clear => SvgIcon::X,
            Self::External => SvgIcon::ExternalLink,
        }
    }
}
