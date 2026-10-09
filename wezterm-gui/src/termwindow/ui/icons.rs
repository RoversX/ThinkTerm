use anyhow::{Context, Result};
use window::Image;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SvgIcon {
    Activity,
    Archive,
    ArchiveRestore,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    Bell,
    Bot,
    Braces,
    Bug,
    Calendar,
    ChartCandlestick,
    ChartLine,
    Check,
    ChevronDown,
    ChevronRight,
    Cloud,
    CodeXml,
    ClipboardPaste,
    Copy,
    Cpu,
    Database,
    Download,
    Ellipsis,
    Expand,
    Eye,
    EyeOff,
    ExternalLink,
    File,
    FileCode,
    FileText,
    Folder,
    FolderMinus,
    FolderOpen,
    FolderPlus,
    FolderTree,
    Gauge,
    GitBranch,
    GitCompare,
    Globe,
    Grid2x2,
    House,
    Info,
    Keyboard,
    Layers,
    Link2,
    Loader,
    LoaderCircle,
    ListChecks,
    ListTodo,
    Maximize2,
    MemoryStick,
    MessageCircle,
    Minimize2,
    Minus,
    NotebookTabs,
    Palette,
    PanelLeft,
    PanelLeftClose,
    PanelLeftOpen,
    PanelRightClose,
    PanelRightOpen,
    Package,
    Pencil,
    Pin,
    PinOff,
    Pipette,
    Plus,
    Puzzle,
    RotateCcw,
    RefreshCw,
    Redo,
    Save,
    Scale,
    Scissors,
    Search,
    Server,
    Settings,
    Shield,
    SlidersHorizontal,
    SlidersVertical,
    Shrink,
    CircleAlert,
    CircleCheck,
    CirclePlus,
    SquarePlus,
    Square,
    SquareStack,
    SquareTerminal,
    SplitHorizontal,
    SplitVertical,
    SpellCheck,
    Terminal,
    Trash2,
    Unlink2,
    ALargeSmall,
    AppWindow,
    Bold,
    CircleStop,
    Columns2,
    Contrast,
    FileCog,
    MessageSquareQuote,
    Mouse,
    MoveVertical,
    Power,
    RotateCw,
    Shuffle,
    Timer,
    Type,
    Laptop,
    QrCode,
    Smartphone,
    Upload,
    Wifi,
    X,
    // Coding-agent brand marks whose logo is monochrome by design, from
    // lobe-icons rather than lucide (see third_party/lobe-icons/README).
    // Being monochrome they take the caller's tint, so they invert with
    // the theme for free. Agents whose logo is genuinely colored live in
    // `BrandIcon` instead; Kimi is in both, because its colored mark is
    // white and vanishes on a light background.
    AgentCodex,
    AgentCursor,
    AgentKimi,
    AgentOpenCode,
    AgentPi,
    // VS Code's mark, from the Material Icon Theme's file icons. Its own
    // blue is ignored like any glyph's colour: it takes the caller's tint.
    VsCode,
}

impl SvgIcon {
    /// The icon a plugin's field names: one of those
    /// `thinkterm_plugin_panel::FIELD_ICONS` lists.
    pub fn for_field(name: &str) -> Option<Self> {
        match name {
            "search" => Some(Self::Search),
            "plus" => Some(Self::Plus),
            "pencil" => Some(Self::Pencil),
            _ => None,
        }
    }

    /// The icon a plugin's panel names: one of those
    /// `thinkterm_plugin_panel::ICONS` lists, else the puzzle piece.
    pub fn for_panel(name: &str) -> Self {
        match name {
            "activity" => Self::Activity,
            "bug" => Self::Bug,
            "calendar" => Self::Calendar,
            "chart-candlestick" => Self::ChartCandlestick,
            "chart-line" => Self::ChartLine,
            "cpu" => Self::Cpu,
            "database" => Self::Database,
            "gauge" => Self::Gauge,
            "git-branch" => Self::GitBranch,
            "git-compare" => Self::GitCompare,
            "list-todo" => Self::ListTodo,
            _ => Self::Puzzle,
        }
    }

    pub fn bytes(self) -> &'static [u8] {
        match self {
            Self::Activity => include_bytes!("../../../../third_party/lucide/icons/activity.svg"),
            Self::Bug => include_bytes!("../../../../third_party/lucide/icons/bug.svg"),
            Self::Calendar => include_bytes!("../../../../third_party/lucide/icons/calendar.svg"),
            Self::ChartCandlestick => {
                include_bytes!("../../../../third_party/lucide/icons/chart-candlestick.svg")
            }
            Self::ChartLine => {
                include_bytes!("../../../../third_party/lucide/icons/chart-line.svg")
            }
            Self::Cpu => include_bytes!("../../../../third_party/lucide/icons/cpu.svg"),
            Self::Database => include_bytes!("../../../../third_party/lucide/icons/database.svg"),
            Self::Gauge => include_bytes!("../../../../third_party/lucide/icons/gauge.svg"),
            Self::GitBranch => {
                include_bytes!("../../../../third_party/lucide/icons/git-branch.svg")
            }
            Self::GitCompare => {
                include_bytes!("../../../../third_party/lucide/icons/git-compare.svg")
            }
            Self::ListTodo => include_bytes!("../../../../third_party/lucide/icons/list-todo.svg"),
            Self::Puzzle => include_bytes!("../../../../third_party/lucide/icons/puzzle.svg"),
            Self::Archive => include_bytes!("../../../../third_party/lucide/icons/archive.svg"),
            Self::ArchiveRestore => {
                include_bytes!("../../../../third_party/lucide/icons/archive-restore.svg")
            }
            Self::ArrowDown => {
                include_bytes!("../../../../third_party/lucide/icons/arrow-down.svg")
            }
            Self::ArrowLeft => {
                include_bytes!("../../../../third_party/lucide/icons/arrow-left.svg")
            }
            Self::ArrowRight => {
                include_bytes!("../../../../third_party/lucide/icons/arrow-right.svg")
            }
            Self::ArrowUp => {
                include_bytes!("../../../../third_party/lucide/icons/arrow-up.svg")
            }
            Self::Bell => include_bytes!("../../../../third_party/lucide/icons/bell.svg"),
            Self::Bot => include_bytes!("../../../../third_party/lucide/icons/bot.svg"),
            Self::Braces => include_bytes!("../../../../third_party/lucide/icons/braces.svg"),
            Self::Check => include_bytes!("../../../../third_party/lucide/icons/check.svg"),
            Self::ChevronDown => {
                include_bytes!("../../../../third_party/lucide/icons/chevron-down.svg")
            }
            Self::ChevronRight => {
                include_bytes!("../../../../third_party/lucide/icons/chevron-right.svg")
            }
            Self::Cloud => include_bytes!("../../../../third_party/lucide/icons/cloud.svg"),
            Self::CodeXml => include_bytes!("../../../../third_party/lucide/icons/code-xml.svg"),
            Self::ClipboardPaste => {
                include_bytes!("../../../../third_party/lucide/icons/clipboard-paste.svg")
            }
            Self::Copy => include_bytes!("../../../../third_party/lucide/icons/copy.svg"),
            Self::Download => {
                include_bytes!("../../../../third_party/lucide/icons/download.svg")
            }
            Self::Ellipsis => include_bytes!("../../../../third_party/lucide/icons/ellipsis.svg"),
            Self::Expand => include_bytes!("../../../../third_party/lucide/icons/expand.svg"),
            Self::Eye => include_bytes!("../../../../third_party/lucide/icons/eye.svg"),
            Self::EyeOff => include_bytes!("../../../../third_party/lucide/icons/eye-off.svg"),
            Self::ExternalLink => {
                include_bytes!("../../../../third_party/lucide/icons/external-link.svg")
            }
            Self::File => include_bytes!("../../../../third_party/lucide/icons/file.svg"),
            Self::FileCode => {
                include_bytes!("../../../../third_party/lucide/icons/file-code.svg")
            }
            Self::FileText => {
                include_bytes!("../../../../third_party/lucide/icons/file-text.svg")
            }
            Self::Folder => include_bytes!("../../../../third_party/lucide/icons/folder.svg"),
            Self::FolderMinus => {
                include_bytes!("../../../../third_party/lucide/icons/folder-minus.svg")
            }
            Self::FolderOpen => {
                include_bytes!("../../../../third_party/lucide/icons/folder-open.svg")
            }
            Self::FolderPlus => {
                include_bytes!("../../../../third_party/lucide/icons/folder-plus.svg")
            }
            Self::FolderTree => {
                include_bytes!("../../../../third_party/lucide/icons/folder-tree.svg")
            }
            Self::Globe => include_bytes!("../../../../third_party/lucide/icons/globe.svg"),
            Self::Grid2x2 => include_bytes!("../../../../third_party/lucide/icons/grid-2x2.svg"),
            Self::House => include_bytes!("../../../../third_party/lucide/icons/house.svg"),
            Self::Info => include_bytes!("../../../../third_party/lucide/icons/info.svg"),
            Self::Keyboard => include_bytes!("../../../../third_party/lucide/icons/keyboard.svg"),
            Self::Layers => include_bytes!("../../../../third_party/lucide/icons/layers.svg"),
            Self::Link2 => include_bytes!("../../../../third_party/lucide/icons/link-2.svg"),
            Self::Loader => include_bytes!("../../../../third_party/lucide/icons/loader.svg"),
            Self::LoaderCircle => {
                include_bytes!("../../../../third_party/lucide/icons/loader-circle.svg")
            }
            Self::ListChecks => {
                include_bytes!("../../../../third_party/lucide/icons/list-checks.svg")
            }
            Self::Maximize2 => {
                include_bytes!("../../../../third_party/lucide/icons/maximize-2.svg")
            }
            Self::MemoryStick => {
                include_bytes!("../../../../third_party/lucide/icons/memory-stick.svg")
            }
            Self::MessageCircle => {
                include_bytes!("../../../../third_party/lucide/icons/message-circle.svg")
            }
            Self::Minimize2 => {
                include_bytes!("../../../../third_party/lucide/icons/minimize-2.svg")
            }
            Self::Minus => include_bytes!("../../../../third_party/lucide/icons/minus.svg"),
            Self::NotebookTabs => {
                include_bytes!("../../../../third_party/lucide/icons/notebook-tabs.svg")
            }
            Self::Palette => include_bytes!("../../../../third_party/lucide/icons/palette.svg"),
            Self::PanelLeft => {
                include_bytes!("../../../../third_party/lucide/icons/panel-left.svg")
            }
            Self::PanelLeftClose => {
                include_bytes!("../../../../third_party/lucide/icons/panel-left-close.svg")
            }
            Self::PanelLeftOpen => {
                include_bytes!("../../../../third_party/lucide/icons/panel-left-open.svg")
            }
            Self::PanelRightClose => {
                include_bytes!("../../../../third_party/lucide/icons/panel-right-close.svg")
            }
            Self::PanelRightOpen => {
                include_bytes!("../../../../third_party/lucide/icons/panel-right-open.svg")
            }
            Self::Pencil => include_bytes!("../../../../third_party/lucide/icons/pencil.svg"),
            Self::Package => include_bytes!("../../../../third_party/lucide/icons/package.svg"),
            Self::Pin => include_bytes!("../../../../third_party/lucide/icons/pin.svg"),
            Self::PinOff => include_bytes!("../../../../third_party/lucide/icons/pin-off.svg"),
            Self::Pipette => include_bytes!("../../../../third_party/lucide/icons/pipette.svg"),
            Self::Unlink2 => include_bytes!("../../../../third_party/lucide/icons/unlink-2.svg"),
            Self::Plus => include_bytes!("../../../../third_party/lucide/icons/plus.svg"),
            Self::RotateCcw => {
                include_bytes!("../../../../third_party/lucide/icons/rotate-ccw.svg")
            }
            Self::RefreshCw => {
                include_bytes!("../../../../third_party/lucide/icons/refresh-cw.svg")
            }
            Self::Redo => include_bytes!("../../../../third_party/lucide/icons/redo.svg"),
            Self::Save => include_bytes!("../../../../third_party/lucide/icons/save.svg"),
            Self::Scale => include_bytes!("../../../../third_party/lucide/icons/scale.svg"),
            Self::Scissors => include_bytes!("../../../../third_party/lucide/icons/scissors.svg"),
            Self::Search => include_bytes!("../../../../third_party/lucide/icons/search.svg"),
            Self::Server => include_bytes!("../../../../third_party/lucide/icons/server.svg"),
            Self::Settings => include_bytes!("../../../../third_party/lucide/icons/settings.svg"),
            Self::Shield => include_bytes!("../../../../third_party/lucide/icons/shield.svg"),
            Self::SlidersHorizontal => {
                include_bytes!("../../../../third_party/lucide/icons/sliders-horizontal.svg")
            }
            Self::SlidersVertical => {
                include_bytes!("../../../../third_party/lucide/icons/sliders-vertical.svg")
            }
            Self::Shrink => include_bytes!("../../../../third_party/lucide/icons/shrink.svg"),
            Self::CircleAlert => {
                include_bytes!("../../../../third_party/lucide/icons/circle-alert.svg")
            }
            Self::CircleCheck => {
                include_bytes!("../../../../third_party/lucide/icons/circle-check.svg")
            }
            Self::CirclePlus => {
                include_bytes!("../../../../third_party/lucide/icons/circle-plus.svg")
            }
            Self::Square => include_bytes!("../../../../third_party/lucide/icons/square.svg"),
            Self::SquarePlus => {
                include_bytes!("../../../../third_party/lucide/icons/square-plus.svg")
            }
            Self::SquareStack => {
                include_bytes!("../../../../third_party/lucide/icons/square-stack.svg")
            }
            Self::SquareTerminal => {
                include_bytes!("../../../../third_party/lucide/icons/square-terminal.svg")
            }
            Self::SplitHorizontal => {
                include_bytes!("../../../../third_party/lucide/icons/square-split-horizontal.svg")
            }
            Self::SplitVertical => {
                include_bytes!("../../../../third_party/lucide/icons/square-split-vertical.svg")
            }
            Self::SpellCheck => {
                include_bytes!("../../../../third_party/lucide/icons/spell-check.svg")
            }
            Self::Terminal => include_bytes!("../../../../third_party/lucide/icons/terminal.svg"),
            Self::Trash2 => include_bytes!("../../../../third_party/lucide/icons/trash-2.svg"),
            Self::ALargeSmall => {
                include_bytes!("../../../../third_party/lucide/icons/a-large-small.svg")
            }
            Self::AppWindow => {
                include_bytes!("../../../../third_party/lucide/icons/app-window.svg")
            }
            Self::Bold => include_bytes!("../../../../third_party/lucide/icons/bold.svg"),
            Self::CircleStop => {
                include_bytes!("../../../../third_party/lucide/icons/circle-stop.svg")
            }
            Self::Columns2 => include_bytes!("../../../../third_party/lucide/icons/columns-2.svg"),
            Self::Contrast => include_bytes!("../../../../third_party/lucide/icons/contrast.svg"),
            Self::FileCog => include_bytes!("../../../../third_party/lucide/icons/file-cog.svg"),
            Self::MessageSquareQuote => {
                include_bytes!("../../../../third_party/lucide/icons/message-square-quote.svg")
            }
            Self::Mouse => include_bytes!("../../../../third_party/lucide/icons/mouse.svg"),
            Self::MoveVertical => {
                include_bytes!("../../../../third_party/lucide/icons/move-vertical.svg")
            }
            Self::Power => include_bytes!("../../../../third_party/lucide/icons/power.svg"),
            Self::RotateCw => include_bytes!("../../../../third_party/lucide/icons/rotate-cw.svg"),
            Self::Shuffle => include_bytes!("../../../../third_party/lucide/icons/shuffle.svg"),
            Self::Timer => include_bytes!("../../../../third_party/lucide/icons/timer.svg"),
            Self::Type => include_bytes!("../../../../third_party/lucide/icons/type.svg"),
            Self::Laptop => include_bytes!("../../../../third_party/lucide/icons/laptop.svg"),
            Self::QrCode => include_bytes!("../../../../third_party/lucide/icons/qr-code.svg"),
            Self::Smartphone => {
                include_bytes!("../../../../third_party/lucide/icons/smartphone.svg")
            }
            Self::Upload => include_bytes!("../../../../third_party/lucide/icons/upload.svg"),
            Self::Wifi => include_bytes!("../../../../third_party/lucide/icons/wifi.svg"),
            Self::X => include_bytes!("../../../../third_party/lucide/icons/x.svg"),
            Self::AgentCodex => include_bytes!("../../../../third_party/lobe-icons/openai.svg"),
            Self::AgentCursor => {
                include_bytes!("../../../../third_party/lobe-icons/cursor.svg")
            }
            Self::AgentKimi => include_bytes!("../../../../third_party/lobe-icons/kimi.svg"),
            Self::AgentOpenCode => {
                include_bytes!("../../../../third_party/lobe-icons/opencode.svg")
            }
            Self::AgentPi => include_bytes!("../../../../third_party/lobe-icons/pi.svg"),
            Self::VsCode => {
                include_bytes!("../../../../third_party/material-icon-theme/icons/vscode.svg")
            }
        }
    }

    pub fn rasterize(self, size: usize) -> Result<Image> {
        self.rasterize_with_rotation(size, 0.0)
    }

    pub fn rasterize_with_rotation(self, size: usize, degrees: f32) -> Result<Image> {
        let svg = std::str::from_utf8(self.bytes()).context("SVG asset is not UTF-8")?;
        // Lucide icons use `currentColor`; tint them white for the dark UI.
        let svg = svg.replace("currentColor", "#ffffff");
        rasterize_svg_str(&svg, size, degrees)
    }
}

/// A full-color file/folder icon from the vendored Material Icon Theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaterialIcon(pub(crate) u16);

mod material_icons_generated {
    include!(concat!(env!("OUT_DIR"), "/material_icons_generated.rs"));
}

impl MaterialIcon {
    pub fn bytes(self) -> &'static [u8] {
        material_icons_generated::material_icon_bytes(self)
    }

    pub fn rasterize(self, size: usize) -> Result<Image> {
        let svg = std::str::from_utf8(self.bytes()).context("material SVG asset is not UTF-8")?;
        rasterize_svg_str(svg, size, 0.0)
    }
}

pub fn material_file_icon_for_name(name: &str) -> Option<MaterialIcon> {
    let key = name.to_ascii_lowercase();
    if let Some(icon) = material_icons_generated::FILE_NAMES
        .get(key.as_str())
        .copied()
    {
        return Some(icon);
    }

    for extension in material_icons_generated::FILE_EXTENSION_SUFFIXES {
        if key
            .strip_suffix(extension)
            .is_some_and(|prefix| prefix.ends_with('.'))
        {
            if let Some(icon) = material_icons_generated::FILE_EXTENSIONS
                .get(*extension)
                .copied()
            {
                return Some(icon);
            }
        }
    }

    material_icons_generated::DEFAULT_FILE
}

pub fn material_folder_icon_for_name(
    name: &str,
    expanded: bool,
    root: bool,
) -> Option<MaterialIcon> {
    let key = name.to_ascii_lowercase();
    if root {
        if expanded {
            if let Some(icon) = material_icons_generated::ROOT_FOLDER_NAMES_EXPANDED
                .get(key.as_str())
                .copied()
            {
                return Some(icon);
            }
        }
        if let Some(icon) = material_icons_generated::ROOT_FOLDER_NAMES
            .get(key.as_str())
            .copied()
        {
            return Some(icon);
        }
    }

    if expanded {
        if let Some(icon) = material_icons_generated::FOLDER_NAMES_EXPANDED
            .get(key.as_str())
            .copied()
        {
            return Some(icon);
        }
    }
    if let Some(icon) = material_icons_generated::FOLDER_NAMES
        .get(key.as_str())
        .copied()
    {
        return Some(icon);
    }

    if root {
        if expanded {
            material_icons_generated::DEFAULT_ROOT_FOLDER_EXPANDED
                .or(material_icons_generated::DEFAULT_ROOT_FOLDER)
                .or(material_icons_generated::DEFAULT_FOLDER_EXPANDED)
                .or(material_icons_generated::DEFAULT_FOLDER)
        } else {
            material_icons_generated::DEFAULT_ROOT_FOLDER
                .or(material_icons_generated::DEFAULT_FOLDER)
        }
    } else if expanded {
        material_icons_generated::DEFAULT_FOLDER_EXPANDED
            .or(material_icons_generated::DEFAULT_FOLDER)
    } else {
        material_icons_generated::DEFAULT_FOLDER
    }
}

/// Brand / product logos painted in full color.
///
/// Most come from the `simple-icons` submodule
/// (`third_party/simple-icons/icons/<slug>.svg`): a single fill-less
/// `<path>` that defaults to black, plus an official brand color which we
/// inject as the path fill. Because many brands are black/near-black
/// (GitHub, Anthropic, Apple, Rust, Debian) and would vanish on the dark
/// UI, a brand color darker than [`MIN_BRAND_LUMA`] is rendered white
/// instead.
///
/// The `Agent*` marks come from `third_party/lobe-icons` and are the
/// exception: their SVGs already carry their own fills and gradients, so
/// they are rasterized untouched — see [`BrandIcon::has_embedded_color`].
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrandIcon {
    AgentClaude,
    AgentCopilot,
    AgentKimi,
    Alpine,
    Anthropic,
    Apple,
    ArchLinux,
    CentOS,
    Claude,
    Cursor,
    Debian,
    Docker,
    Fedora,
    Git,
    GitHub,
    Herdr,
    Linux,
    NixOS,
    Proxmox,
    RaspberryPi,
    RedHat,
    Ubuntu,
    VSCodium,
    WezTerm,
    Windsurf,
}

/// Brand colors darker than this perceived luminance (0..=255) are rendered
/// white so they stay visible on ThinkTerm's dark chrome.
const MIN_BRAND_LUMA: f32 = 40.0;

impl BrandIcon {
    /// The mark an import source names in its `SourceInfo::icon`.
    #[cfg(unix)]
    pub fn for_import_source(icon: &str) -> Option<Self> {
        match icon {
            "herdr" => Some(Self::Herdr),
            _ => None,
        }
    }

    pub fn bytes(self) -> &'static [u8] {
        match self {
            Self::AgentClaude => {
                include_bytes!("../../../../third_party/lobe-icons/claude-color.svg")
            }
            Self::AgentCopilot => {
                include_bytes!("../../../../third_party/lobe-icons/copilot-color.svg")
            }
            Self::AgentKimi => {
                include_bytes!("../../../../third_party/lobe-icons/kimi-color.svg")
            }
            Self::Alpine => {
                include_bytes!("../../../../third_party/simple-icons/icons/alpinelinux.svg")
            }
            Self::Anthropic => {
                include_bytes!("../../../../third_party/simple-icons/icons/anthropic.svg")
            }
            Self::Apple => include_bytes!("../../../../third_party/simple-icons/icons/apple.svg"),
            Self::ArchLinux => {
                include_bytes!("../../../../third_party/simple-icons/icons/archlinux.svg")
            }
            Self::CentOS => include_bytes!("../../../../third_party/simple-icons/icons/centos.svg"),
            Self::Claude => include_bytes!("../../../../third_party/simple-icons/icons/claude.svg"),
            Self::Cursor => include_bytes!("../../../../third_party/simple-icons/icons/cursor.svg"),
            Self::Debian => include_bytes!("../../../../third_party/simple-icons/icons/debian.svg"),
            Self::Docker => include_bytes!("../../../../third_party/simple-icons/icons/docker.svg"),
            Self::Fedora => include_bytes!("../../../../third_party/simple-icons/icons/fedora.svg"),
            Self::Git => include_bytes!("../../../../third_party/simple-icons/icons/git.svg"),
            Self::GitHub => include_bytes!("../../../../third_party/simple-icons/icons/github.svg"),
            Self::Herdr => include_bytes!("../../../../third_party/herdr/logo.svg"),
            Self::Linux => include_bytes!("../../../../third_party/simple-icons/icons/linux.svg"),
            Self::NixOS => include_bytes!("../../../../third_party/simple-icons/icons/nixos.svg"),
            Self::Proxmox => {
                include_bytes!("../../../../third_party/simple-icons/icons/proxmox.svg")
            }
            Self::RaspberryPi => {
                include_bytes!("../../../../third_party/simple-icons/icons/raspberrypi.svg")
            }
            Self::RedHat => include_bytes!("../../../../third_party/simple-icons/icons/redhat.svg"),
            Self::Ubuntu => include_bytes!("../../../../third_party/simple-icons/icons/ubuntu.svg"),
            Self::VSCodium => {
                include_bytes!("../../../../third_party/simple-icons/icons/vscodium.svg")
            }
            Self::WezTerm => include_bytes!("../../../../assets/icon/wezterm-icon.svg"),
            Self::Windsurf => {
                include_bytes!("../../../../third_party/simple-icons/icons/windsurf.svg")
            }
        }
    }

    /// Whether this mark's SVG already carries its own fills, so the
    /// single-path color injection below must be skipped. Injecting onto a
    /// path that already has a `fill` produces a duplicate XML attribute,
    /// which usvg rejects outright — the icon would not merely look wrong,
    /// it would fail to rasterize and take the frame down with it.
    fn has_embedded_color(self) -> bool {
        matches!(
            self,
            Self::AgentClaude | Self::AgentCopilot | Self::AgentKimi | Self::Herdr | Self::WezTerm
        )
    }

    /// Official brand color (sRGB) from `simple-icons/data/simple-icons.json`.
    /// Unused for [`Self::has_embedded_color`] marks, which keep their own;
    /// the entries below are their dominant color, for reference.
    pub fn brand_color(self) -> (u8, u8, u8) {
        match self {
            Self::AgentClaude => (0xD9, 0x77, 0x57),
            Self::AgentCopilot => (0x24, 0x96, 0xED),
            Self::AgentKimi => (0x17, 0x83, 0xFF),
            Self::Alpine => (0x0D, 0x59, 0x7F),
            Self::Anthropic => (0x19, 0x19, 0x19),
            Self::Apple => (0x00, 0x00, 0x00),
            Self::ArchLinux => (0x17, 0x93, 0xD1),
            Self::CentOS => (0x26, 0x25, 0x77),
            Self::Claude => (0xD9, 0x77, 0x57),
            Self::Cursor => (0x00, 0x00, 0x00),
            Self::Debian => (0xA8, 0x1D, 0x33),
            Self::Docker => (0x24, 0x96, 0xED),
            Self::Fedora => (0x51, 0xA2, 0xDA),
            Self::Git => (0xF0, 0x3C, 0x2E),
            Self::GitHub => (0x18, 0x17, 0x17),
            Self::Herdr => (0x30, 0x34, 0x38),
            Self::Linux => (0xFC, 0xC6, 0x24),
            Self::NixOS => (0x52, 0x77, 0xC3),
            Self::Proxmox => (0xE5, 0x70, 0x00),
            Self::RaspberryPi => (0xA2, 0x28, 0x46),
            Self::RedHat => (0xEE, 0x00, 0x00),
            Self::Ubuntu => (0xE9, 0x54, 0x20),
            Self::VSCodium => (0x2F, 0x80, 0xED),
            Self::WezTerm => (0x4E, 0x49, 0xEE),
            Self::Windsurf => (0x0B, 0x10, 0x0F),
        }
    }

    /// Brand color, falling back to white when too dark for the dark UI.
    pub fn display_color(self) -> (u8, u8, u8) {
        let (r, g, b) = self.brand_color();
        let luma = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
        if luma < MIN_BRAND_LUMA {
            (0xFF, 0xFF, 0xFF)
        } else {
            (r, g, b)
        }
    }

    pub fn rasterize(self, size: usize) -> Result<Image> {
        let svg = std::str::from_utf8(self.bytes()).context("brand SVG asset is not UTF-8")?;
        if matches!(self, Self::Herdr | Self::WezTerm) {
            let root = svg.find("<svg").context("brand SVG root is missing")?;
            let start = root
                + svg[root..]
                    .find('>')
                    .context("brand SVG root is incomplete")?
                + 1;
            let end = svg.rfind("</svg>").context("brand SVG end is missing")?;
            let clipped = format!(
                r#"{}<defs><clipPath id="app-icon" clipPathUnits="objectBoundingBox"><rect width="1" height="1" rx="0.27"/></clipPath></defs><g clip-path="url(#app-icon)">{}</g></svg>"#,
                &svg[..start],
                &svg[start..end],
            );
            return rasterize_svg_str(&clipped, size, 0.0);
        }
        if self.has_embedded_color() {
            return rasterize_svg_str(svg, size, 0.0);
        }
        let (r, g, b) = self.display_color();
        let fill = format!("#{r:02X}{g:02X}{b:02X}");
        // simple-icons paths carry no fill; inject the brand color on the
        // single `<path ` element so resvg renders it in color.
        let svg = svg.replacen("<path ", &format!("<path fill=\"{fill}\" "), 1);
        rasterize_svg_str(&svg, size, 0.0)
    }
}

/// Map an `/etc/os-release` `ID` (or a value from `ID_LIKE`) to a brand icon.
/// Returns `None` when the distro is unknown so callers can fall back to a
/// generic server icon.
pub fn distro_to_icon(id: &str) -> Option<BrandIcon> {
    match id.trim().to_ascii_lowercase().as_str() {
        "ubuntu" => Some(BrandIcon::Ubuntu),
        "debian" => Some(BrandIcon::Debian),
        "raspbian" => Some(BrandIcon::RaspberryPi),
        "fedora" => Some(BrandIcon::Fedora),
        "arch" | "archlinux" | "arch-linux" => Some(BrandIcon::ArchLinux),
        "alpine" => Some(BrandIcon::Alpine),
        "rhel" | "redhat" | "red hat" => Some(BrandIcon::RedHat),
        "centos" => Some(BrandIcon::CentOS),
        "nixos" => Some(BrandIcon::NixOS),
        "proxmox" | "pve" => Some(BrandIcon::Proxmox),
        "darwin" | "macos" => Some(BrandIcon::Apple),
        "linux" => Some(BrandIcon::Linux),
        _ => None,
    }
}

/// Shared SVG -> RGBA raster path used by both [`SvgIcon`] and [`BrandIcon`].
/// The icon is scaled to fit and centered within a `size`×`size` pixmap.
/// The application icon, for the About page's hero.
///
/// The only raster asset in here. `image` hands back straight alpha while the
/// atlas stores premultiplied -- the multiply below is what stops the icon's
/// soft edges from ringing bright against a dark card.
pub fn rasterize_app_icon(size: usize) -> Result<Image> {
    rasterize_png_icon(crate::termwindow::ICON_DATA, size)
}

/// `png` scaled to `size` square and premultiplied for the atlas, the way
/// `rasterize_app_icon` does the About page's icon.
pub fn rasterize_png_icon(png: &[u8], size: usize) -> Result<Image> {
    let size = size.max(1);
    let decoded = image::load_from_memory(png)
        .context("decoding an application icon")?
        .resize_exact(
            size as u32,
            size as u32,
            image::imageops::FilterType::Lanczos3,
        )
        .into_rgba8();
    let mut data = decoded.into_raw();
    for pixel in data.chunks_exact_mut(4) {
        let alpha = pixel[3] as u32;
        for channel in &mut pixel[..3] {
            *channel = ((*channel as u32 * alpha + 127) / 255) as u8;
        }
    }
    Ok(Image::from_raw(size, size, data))
}

/// How far down the CloseX logo its wordmark's baseline sits, as a fraction
/// of the logo's height: 190 of the SVG's 234 units.
pub const CLOSEX_LOGO_BASELINE: f32 = 190.0 / 234.0;

/// The CloseX logo at the foot of the About page, `height` pixels tall and as
/// wide as its proportions make it. White on transparent like the Lucide set,
/// so it is drawn as a tinted mask and follows the theme.
pub fn rasterize_closex_logo(height: usize) -> Result<Image> {
    let svg = include_bytes!("../../../../assets/icon/CloseX.svg");
    let tree = resvg::usvg::Tree::from_data(svg, &resvg::usvg::Options::default())
        .context("parsing the CloseX logo")?;
    let svg_size = tree.size();
    let height = height.max(1);
    let scale = height as f32 / svg_size.height();
    let width = ((svg_size.width() * scale).round() as usize).max(1);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width as u32, height as u32)
        .with_context(|| format!("allocating {width}x{height}px logo pixmap"))?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(Image::from_raw(width, height, pixmap.take()))
}

fn rasterize_svg_str(svg: &str, size: usize, degrees: f32) -> Result<Image> {
    let size = size.max(1);
    let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
        .context("parsing SVG icon")?;
    let svg_size = tree.size();
    let scale = (size as f32 / svg_size.width()).min(size as f32 / svg_size.height());
    let translate_x = (size as f32 - svg_size.width() * scale) / 2.0;
    let translate_y = (size as f32 - svg_size.height() * scale) / 2.0;
    let mut transform = resvg::tiny_skia::Transform::from_translate(translate_x, translate_y)
        .pre_scale(scale, scale);
    if degrees != 0.0 {
        transform = transform.post_rotate_at(degrees, size as f32 / 2.0, size as f32 / 2.0);
    }

    let mut pixmap = resvg::tiny_skia::Pixmap::new(size as u32, size as u32)
        .with_context(|| format!("allocating {size}px SVG icon pixmap"))?;
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    Ok(Image::from_raw(size, size, pixmap.take()))
}

#[cfg(test)]
mod tests {
    use super::{material_file_icon_for_name, material_folder_icon_for_name, BrandIcon, SvgIcon};

    #[test]
    fn svg_icons_rasterize() {
        for icon in [
            SvgIcon::Archive,
            SvgIcon::ArchiveRestore,
            SvgIcon::ArrowLeft,
            SvgIcon::Bell,
            SvgIcon::Braces,
            SvgIcon::Server,
            SvgIcon::ExternalLink,
            SvgIcon::ChevronDown,
            SvgIcon::ChevronRight,
            SvgIcon::Cloud,
            SvgIcon::CodeXml,
            SvgIcon::Copy,
            SvgIcon::Expand,
            SvgIcon::Folder,
            SvgIcon::FolderOpen,
            SvgIcon::FolderPlus,
            SvgIcon::FolderTree,
            SvgIcon::Globe,
            SvgIcon::Info,
            SvgIcon::Keyboard,
            SvgIcon::Loader,
            SvgIcon::LoaderCircle,
            SvgIcon::ListChecks,
            SvgIcon::Maximize2,
            SvgIcon::MessageCircle,
            SvgIcon::Minus,
            SvgIcon::Minimize2,
            SvgIcon::NotebookTabs,
            SvgIcon::Palette,
            SvgIcon::PanelLeft,
            SvgIcon::PanelLeftClose,
            SvgIcon::PanelLeftOpen,
            SvgIcon::PanelRightClose,
            SvgIcon::PanelRightOpen,
            SvgIcon::Pin,
            SvgIcon::PinOff,
            SvgIcon::Pipette,
            SvgIcon::Plus,
            SvgIcon::Unlink2,
            SvgIcon::RotateCcw,
            SvgIcon::Redo,
            SvgIcon::Search,
            SvgIcon::Settings,
            SvgIcon::SlidersHorizontal,
            SvgIcon::SlidersVertical,
            SvgIcon::Shrink,
            SvgIcon::CircleAlert,
            SvgIcon::CircleCheck,
            SvgIcon::CirclePlus,
            SvgIcon::Square,
            SvgIcon::SquareTerminal,
            SvgIcon::SplitHorizontal,
            SvgIcon::SplitVertical,
            SvgIcon::Terminal,
            SvgIcon::Trash2,
            SvgIcon::X,
            // What a plugin's panel is shown by in the selector.
            SvgIcon::Activity,
            SvgIcon::Bug,
            SvgIcon::Calendar,
            SvgIcon::ChartCandlestick,
            SvgIcon::ChartLine,
            SvgIcon::Cpu,
            SvgIcon::Database,
            SvgIcon::Gauge,
            SvgIcon::GitBranch,
            SvgIcon::GitCompare,
            SvgIcon::ListTodo,
            SvgIcon::Puzzle,
            // The agent marks come from a different upstream than the
            // rest, sized in `em` rather than pixels: keep them covered so
            // a re-vendored file that usvg cannot size fails here.
            SvgIcon::AgentCodex,
            SvgIcon::AgentCursor,
            SvgIcon::AgentKimi,
            SvgIcon::AgentOpenCode,
            SvgIcon::AgentPi,
            SvgIcon::VsCode,
        ] {
            let data: Vec<u8> = icon.rasterize(24).unwrap().into();
            assert_eq!(data.len(), 24 * 24 * 4);
            assert!(data.iter().any(|value| *value != 0));
        }
    }

    #[test]
    fn brand_icons_rasterize() {
        for icon in [
            // These three take the embedded-color path, where a stray
            // fill injection would be a hard usvg parse error rather than
            // a cosmetic bug — see BrandIcon::has_embedded_color.
            BrandIcon::AgentClaude,
            BrandIcon::AgentCopilot,
            BrandIcon::AgentKimi,
            BrandIcon::Alpine,
            BrandIcon::Anthropic,
            BrandIcon::Apple,
            BrandIcon::ArchLinux,
            BrandIcon::CentOS,
            BrandIcon::Claude,
            BrandIcon::Cursor,
            BrandIcon::Debian,
            BrandIcon::Docker,
            BrandIcon::Fedora,
            BrandIcon::Git,
            BrandIcon::GitHub,
            BrandIcon::Herdr,
            BrandIcon::Linux,
            BrandIcon::NixOS,
            BrandIcon::Proxmox,
            BrandIcon::RaspberryPi,
            BrandIcon::RedHat,
            BrandIcon::Ubuntu,
            BrandIcon::VSCodium,
            BrandIcon::WezTerm,
            BrandIcon::Windsurf,
        ] {
            let data: Vec<u8> = icon.rasterize(24).unwrap().into();
            assert_eq!(data.len(), 24 * 24 * 4);
            // The brand color must actually paint some non-transparent pixels.
            assert!(
                data.iter().any(|value| *value != 0),
                "{:?} rendered empty",
                icon
            );
        }
    }

    #[test]
    fn dark_brands_fall_back_to_white() {
        // GitHub (#181717) is too dark for the dark UI and must render white.
        assert_eq!(BrandIcon::GitHub.display_color(), (0xFF, 0xFF, 0xFF));
        // Ubuntu (#E95420) keeps its brand color.
        assert_eq!(BrandIcon::Ubuntu.display_color(), (0xE9, 0x54, 0x20));
    }

    #[test]
    fn material_file_icons_resolve_common_names_and_suffixes() {
        let package_json = material_file_icon_for_name("package.json").unwrap();
        let plain_json = material_file_icon_for_name("plain.json").unwrap();
        assert_ne!(package_json, plain_json);

        assert!(material_file_icon_for_name("main.rs").is_some());
        assert!(material_file_icon_for_name("Cargo.toml").is_some());
        assert!(material_file_icon_for_name("README.md").is_some());
        assert!(material_file_icon_for_name(".gitignore").is_some());
        assert!(material_file_icon_for_name("component.tsx").is_some());

        let typescript_definition = material_file_icon_for_name("index.d.ts").unwrap();
        let typescript = material_file_icon_for_name("index.ts").unwrap();
        assert_ne!(typescript_definition, typescript);
    }

    #[test]
    fn material_folder_icons_resolve_names_and_expanded_state() {
        let src_closed = material_folder_icon_for_name("src", false, false).unwrap();
        let src_open = material_folder_icon_for_name("src", true, false).unwrap();
        assert_ne!(src_closed, src_open);

        assert!(material_folder_icon_for_name("node_modules", false, false).is_some());
        assert!(material_folder_icon_for_name(".github", true, false).is_some());
        assert!(material_folder_icon_for_name("Project", true, true).is_some());
    }

    #[test]
    fn material_icons_rasterize() {
        let icon = material_file_icon_for_name("main.rs").unwrap();
        let data: Vec<u8> = icon.rasterize(24).unwrap().into();
        assert_eq!(data.len(), 24 * 24 * 4);
        assert!(data.iter().any(|value| *value != 0));
    }

    #[test]
    fn closex_logo_rasterizes_at_its_proportions() {
        use window::BitmapImage;
        let image = super::rasterize_closex_logo(32).unwrap();
        let (width, height) = image.image_dimensions();
        assert_eq!(height, 32);
        assert_eq!(width, 120);
        let data: Vec<u8> = image.into();
        assert!(data.iter().any(|value| *value != 0));
    }
}
