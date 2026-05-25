use anyhow::{Context, Result};
use window::Image;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SvgIcon {
    ExternalLink,
    FolderOpen,
    Globe,
    PanelRightClose,
    PanelRightOpen,
    Plus,
    SquareTerminal,
    SplitHorizontal,
    SplitVertical,
    Terminal,
    X,
}

impl SvgIcon {
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Self::ExternalLink => {
                include_bytes!("../../../../third_party/lucide/icons/external-link.svg")
            }
            Self::FolderOpen => {
                include_bytes!("../../../../third_party/lucide/icons/folder-open.svg")
            }
            Self::Globe => include_bytes!("../../../../third_party/lucide/icons/globe.svg"),
            Self::PanelRightClose => {
                include_bytes!("../../../../third_party/lucide/icons/panel-right-close.svg")
            }
            Self::PanelRightOpen => {
                include_bytes!("../../../../third_party/lucide/icons/panel-right-open.svg")
            }
            Self::Plus => include_bytes!("../../../../third_party/lucide/icons/plus.svg"),
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
            Self::X => include_bytes!("../../../../third_party/lucide/icons/x.svg"),
        }
    }

    pub fn rasterize(self, size: usize) -> Result<Image> {
        let size = size.max(1);
        let svg = std::str::from_utf8(self.bytes()).context("SVG asset is not UTF-8")?;
        let svg = svg.replace("currentColor", "#ffffff");
        let tree = resvg::usvg::Tree::from_data(svg.as_bytes(), &resvg::usvg::Options::default())
            .context("parsing SVG icon")?;
        let svg_size = tree.size();
        let scale = (size as f32 / svg_size.width()).min(size as f32 / svg_size.height());
        let translate_x = (size as f32 - svg_size.width() * scale) / 2.0;
        let translate_y = (size as f32 - svg_size.height() * scale) / 2.0;
        let transform = resvg::tiny_skia::Transform::from_translate(translate_x, translate_y)
            .pre_scale(scale, scale);

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
            SvgIcon::ExternalLink,
            SvgIcon::FolderOpen,
            SvgIcon::Globe,
            SvgIcon::PanelRightClose,
            SvgIcon::PanelRightOpen,
            SvgIcon::Plus,
            SvgIcon::SquareTerminal,
            SvgIcon::SplitHorizontal,
            SvgIcon::SplitVertical,
            SvgIcon::Terminal,
            SvgIcon::X,
        ] {
            let data: Vec<u8> = icon.rasterize(24).unwrap().into();
            assert_eq!(data.len(), 24 * 24 * 4);
            assert!(data.iter().any(|value| *value != 0));
        }
    }
}
