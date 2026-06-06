use anyhow::{Context, Result};
use window::Image;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SvgIcon {
    Archive,
    ArrowLeft,
    Bell,
    Braces,
    Check,
    ChevronDown,
    ChevronRight,
    Cloud,
    CodeXml,
    ClipboardPaste,
    Ellipsis,
    Expand,
    ExternalLink,
    Folder,
    FolderMinus,
    FolderOpen,
    FolderPlus,
    FolderTree,
    Globe,
    House,
    Info,
    Keyboard,
    Layers,
    Link2,
    Loader,
    LoaderCircle,
    ListChecks,
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
    Pencil,
    Pin,
    PinOff,
    Plus,
    RotateCcw,
    Search,
    Server,
    Settings,
    SlidersHorizontal,
    SlidersVertical,
    Shrink,
    CircleAlert,
    CircleCheck,
    CirclePlus,
    SquarePlus,
    SquareStack,
    SquareTerminal,
    SplitHorizontal,
    SplitVertical,
    Terminal,
    Trash2,
    X,
}

impl SvgIcon {
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Self::Archive => include_bytes!("../../../../third_party/lucide/icons/archive.svg"),
            Self::ArrowLeft => {
                include_bytes!("../../../../third_party/lucide/icons/arrow-left.svg")
            }
            Self::Bell => include_bytes!("../../../../third_party/lucide/icons/bell.svg"),
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
            Self::Ellipsis => include_bytes!("../../../../third_party/lucide/icons/ellipsis.svg"),
            Self::Expand => include_bytes!("../../../../third_party/lucide/icons/expand.svg"),
            Self::ExternalLink => {
                include_bytes!("../../../../third_party/lucide/icons/external-link.svg")
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
            Self::Pin => include_bytes!("../../../../third_party/lucide/icons/pin.svg"),
            Self::PinOff => include_bytes!("../../../../third_party/lucide/icons/pin-off.svg"),
            Self::Plus => include_bytes!("../../../../third_party/lucide/icons/plus.svg"),
            Self::RotateCcw => {
                include_bytes!("../../../../third_party/lucide/icons/rotate-ccw.svg")
            }
            Self::Search => include_bytes!("../../../../third_party/lucide/icons/search.svg"),
            Self::Server => include_bytes!("../../../../third_party/lucide/icons/server.svg"),
            Self::Settings => include_bytes!("../../../../third_party/lucide/icons/settings.svg"),
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
            Self::Terminal => include_bytes!("../../../../third_party/lucide/icons/terminal.svg"),
            Self::Trash2 => include_bytes!("../../../../third_party/lucide/icons/trash-2.svg"),
            Self::X => include_bytes!("../../../../third_party/lucide/icons/x.svg"),
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

/// Brand / product logos sourced from the `simple-icons` submodule
/// (`third_party/simple-icons/icons/<slug>.svg`).
///
/// Unlike Lucide, simple-icons SVGs are a single fill-less `<path>` that
/// defaults to black, and each brand has an official color. We inject that
/// color as the path fill. Because many brands are black/near-black (GitHub,
/// Anthropic, Apple, Rust, Debian) and would vanish on the dark UI, a brand
/// color darker than [`MIN_BRAND_LUMA`] is rendered white instead.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BrandIcon {
    Alpine,
    Anthropic,
    Apple,
    ArchLinux,
    CentOS,
    Claude,
    Debian,
    Docker,
    Fedora,
    Git,
    GitHub,
    Linux,
    NixOS,
    Proxmox,
    RaspberryPi,
    RedHat,
    Ubuntu,
}

/// Brand colors darker than this perceived luminance (0..=255) are rendered
/// white so they stay visible on ThinkTerm's dark chrome.
const MIN_BRAND_LUMA: f32 = 40.0;

impl BrandIcon {
    pub fn bytes(self) -> &'static [u8] {
        match self {
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
            Self::Debian => include_bytes!("../../../../third_party/simple-icons/icons/debian.svg"),
            Self::Docker => include_bytes!("../../../../third_party/simple-icons/icons/docker.svg"),
            Self::Fedora => include_bytes!("../../../../third_party/simple-icons/icons/fedora.svg"),
            Self::Git => include_bytes!("../../../../third_party/simple-icons/icons/git.svg"),
            Self::GitHub => include_bytes!("../../../../third_party/simple-icons/icons/github.svg"),
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
        }
    }

    /// Official brand color (sRGB) from `simple-icons/data/simple-icons.json`.
    pub fn brand_color(self) -> (u8, u8, u8) {
        match self {
            Self::Alpine => (0x0D, 0x59, 0x7F),
            Self::Anthropic => (0x19, 0x19, 0x19),
            Self::Apple => (0x00, 0x00, 0x00),
            Self::ArchLinux => (0x17, 0x93, 0xD1),
            Self::CentOS => (0x26, 0x25, 0x77),
            Self::Claude => (0xD9, 0x77, 0x57),
            Self::Debian => (0xA8, 0x1D, 0x33),
            Self::Docker => (0x24, 0x96, 0xED),
            Self::Fedora => (0x51, 0xA2, 0xDA),
            Self::Git => (0xF0, 0x3C, 0x2E),
            Self::GitHub => (0x18, 0x17, 0x17),
            Self::Linux => (0xFC, 0xC6, 0x24),
            Self::NixOS => (0x52, 0x77, 0xC3),
            Self::Proxmox => (0xE5, 0x70, 0x00),
            Self::RaspberryPi => (0xA2, 0x28, 0x46),
            Self::RedHat => (0xEE, 0x00, 0x00),
            Self::Ubuntu => (0xE9, 0x54, 0x20),
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
    use super::{BrandIcon, SvgIcon};

    #[test]
    fn svg_icons_rasterize() {
        for icon in [
            SvgIcon::Archive,
            SvgIcon::ArrowLeft,
            SvgIcon::Bell,
            SvgIcon::Braces,
            SvgIcon::Server,
            SvgIcon::ExternalLink,
            SvgIcon::ChevronDown,
            SvgIcon::ChevronRight,
            SvgIcon::Cloud,
            SvgIcon::CodeXml,
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
            SvgIcon::Plus,
            SvgIcon::RotateCcw,
            SvgIcon::Search,
            SvgIcon::Settings,
            SvgIcon::SlidersHorizontal,
            SvgIcon::SlidersVertical,
            SvgIcon::Shrink,
            SvgIcon::CircleAlert,
            SvgIcon::CircleCheck,
            SvgIcon::CirclePlus,
            SvgIcon::SquareTerminal,
            SvgIcon::SplitHorizontal,
            SvgIcon::SplitVertical,
            SvgIcon::Terminal,
            SvgIcon::Trash2,
            SvgIcon::X,
        ] {
            let data: Vec<u8> = icon.rasterize(24).unwrap().into();
            assert_eq!(data.len(), 24 * 24 * 4);
            assert!(data.iter().any(|value| *value != 0));
        }
    }

    #[test]
    fn brand_icons_rasterize() {
        for icon in [
            BrandIcon::Alpine,
            BrandIcon::Anthropic,
            BrandIcon::Apple,
            BrandIcon::ArchLinux,
            BrandIcon::CentOS,
            BrandIcon::Claude,
            BrandIcon::Debian,
            BrandIcon::Docker,
            BrandIcon::Fedora,
            BrandIcon::Git,
            BrandIcon::GitHub,
            BrandIcon::Linux,
            BrandIcon::NixOS,
            BrandIcon::Proxmox,
            BrandIcon::RaspberryPi,
            BrandIcon::RedHat,
            BrandIcon::Ubuntu,
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
}
