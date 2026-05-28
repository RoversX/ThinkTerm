use anyhow::{Context, Result};
use window::Image;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SvgIcon {
    Archive,
    Bell,
    ChevronDown,
    ChevronRight,
    Cloud,
    Expand,
    ExternalLink,
    Folder,
    FolderOpen,
    FolderPlus,
    Globe,
    Info,
    Keyboard,
    Loader,
    LoaderCircle,
    Maximize2,
    MemoryStick,
    Minimize2,
    Minus,
    Palette,
    PanelLeft,
    PanelLeftClose,
    PanelLeftOpen,
    PanelRightClose,
    PanelRightOpen,
    Pin,
    PinOff,
    Plus,
    RotateCcw,
    Search,
    Settings,
    SlidersHorizontal,
    SlidersVertical,
    Shrink,
    CircleAlert,
    CircleCheck,
    CirclePlus,
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
            Self::Bell => include_bytes!("../../../../third_party/lucide/icons/bell.svg"),
            Self::ChevronDown => {
                include_bytes!("../../../../third_party/lucide/icons/chevron-down.svg")
            }
            Self::ChevronRight => {
                include_bytes!("../../../../third_party/lucide/icons/chevron-right.svg")
            }
            Self::Cloud => include_bytes!("../../../../third_party/lucide/icons/cloud.svg"),
            Self::Expand => include_bytes!("../../../../third_party/lucide/icons/expand.svg"),
            Self::ExternalLink => {
                include_bytes!("../../../../third_party/lucide/icons/external-link.svg")
            }
            Self::Folder => include_bytes!("../../../../third_party/lucide/icons/folder.svg"),
            Self::FolderOpen => {
                include_bytes!("../../../../third_party/lucide/icons/folder-open.svg")
            }
            Self::FolderPlus => {
                include_bytes!("../../../../third_party/lucide/icons/folder-plus.svg")
            }
            Self::Globe => include_bytes!("../../../../third_party/lucide/icons/globe.svg"),
            Self::Info => include_bytes!("../../../../third_party/lucide/icons/info.svg"),
            Self::Keyboard => include_bytes!("../../../../third_party/lucide/icons/keyboard.svg"),
            Self::Loader => include_bytes!("../../../../third_party/lucide/icons/loader.svg"),
            Self::LoaderCircle => {
                include_bytes!("../../../../third_party/lucide/icons/loader-circle.svg")
            }
            Self::Maximize2 => {
                include_bytes!("../../../../third_party/lucide/icons/maximize-2.svg")
            }
            Self::MemoryStick => {
                include_bytes!("../../../../third_party/lucide/icons/memory-stick.svg")
            }
            Self::Minimize2 => {
                include_bytes!("../../../../third_party/lucide/icons/minimize-2.svg")
            }
            Self::Minus => include_bytes!("../../../../third_party/lucide/icons/minus.svg"),
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
            Self::Pin => include_bytes!("../../../../third_party/lucide/icons/pin.svg"),
            Self::PinOff => include_bytes!("../../../../third_party/lucide/icons/pin-off.svg"),
            Self::Plus => include_bytes!("../../../../third_party/lucide/icons/plus.svg"),
            Self::RotateCcw => {
                include_bytes!("../../../../third_party/lucide/icons/rotate-ccw.svg")
            }
            Self::Search => include_bytes!("../../../../third_party/lucide/icons/search.svg"),
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
        let size = size.max(1);
        let svg = std::str::from_utf8(self.bytes()).context("SVG asset is not UTF-8")?;
        let svg = svg.replace("currentColor", "#ffffff");
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
}

#[cfg(test)]
mod tests {
    use super::SvgIcon;

    #[test]
    fn svg_icons_rasterize() {
        for icon in [
            SvgIcon::Archive,
            SvgIcon::Bell,
            SvgIcon::ExternalLink,
            SvgIcon::ChevronDown,
            SvgIcon::ChevronRight,
            SvgIcon::Cloud,
            SvgIcon::Expand,
            SvgIcon::Folder,
            SvgIcon::FolderOpen,
            SvgIcon::FolderPlus,
            SvgIcon::Globe,
            SvgIcon::Info,
            SvgIcon::Keyboard,
            SvgIcon::Loader,
            SvgIcon::LoaderCircle,
            SvgIcon::Maximize2,
            SvgIcon::Minus,
            SvgIcon::Minimize2,
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
}
