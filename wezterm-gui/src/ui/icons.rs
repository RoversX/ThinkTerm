pub(crate) use crate::termwindow::ui::icons::{BrandIcon, SvgIcon};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SettingsIcon {
    General,
    Appearance,
    Accessibility,
    TabIcons,
    Sidebar,
    Terminal,
    Workspaces,
    Agents,
    Keymap,
    Import,
    Developer,
    UiKit,
    Memory,
    Update,
    About,
    Archived,
    Backup,
    Web,
    Share,
    Privacy,
    Tailscale,
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
            Self::Accessibility => SvgIcon::PersonStanding,
            Self::TabIcons => SvgIcon::Grid2x2,
            Self::Sidebar => SvgIcon::PanelRightOpen,
            Self::Terminal => SvgIcon::Terminal,
            Self::Workspaces => SvgIcon::FolderOpen,
            Self::Agents => SvgIcon::Bot,
            Self::Keymap => SvgIcon::Keyboard,
            Self::Import => SvgIcon::Download,
            Self::Developer => SvgIcon::Settings,
            Self::UiKit => SvgIcon::SlidersVertical,
            Self::Memory => SvgIcon::MemoryStick,
            Self::Update => SvgIcon::RefreshCw,
            Self::About => SvgIcon::Info,
            Self::Archived => SvgIcon::Archive,
            Self::Backup => SvgIcon::Save,
            Self::Web => SvgIcon::Globe,
            Self::Share => SvgIcon::Share,
            Self::Privacy => SvgIcon::Shield,
            Self::Tailscale => SvgIcon::Network,
            Self::CommandPalette => SvgIcon::SquareTerminal,
            Self::Search => SvgIcon::Search,
            Self::Clear => SvgIcon::X,
            Self::External => SvgIcon::ExternalLink,
        }
    }
}
