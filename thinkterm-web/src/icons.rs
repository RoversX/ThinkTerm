//! The lucide icons the desktop draws its chrome with, as inline SVG.
//! The same files `wezterm-gui/src/termwindow/ui/icons.rs` embeds; they
//! take their colour from CSS `color` and their size from CSS.

macro_rules! lucide {
    ($($name:literal),* $(,)?) => {
        pub fn svg(name: &str) -> &'static str {
            match name {
                $($name => include_str!(concat!("../../third_party/lucide/icons/", $name, ".svg")),)*
                _ => "",
            }
        }
    };
}

lucide!(
    "square-terminal",
    "x",
    "plus",
    "square-split-horizontal",
    "square-split-vertical",
    "maximize-2",
    "minimize-2",
    "loader-circle",
    "circle-alert",
    "circle-check",
    "panel-left",
    "folder",
    "folder-open",
    "folder-plus",
    "chevron-right",
    "chevron-down",
    "pin",
    "pin-off",
    "trash-2",
    "circle-plus",
    "layers",
    "ellipsis",
    "archive",
    "archive-restore",
    "globe",
    "link-2",
);

#[cfg(test)]
mod tests {
    #[test]
    fn every_named_icon_is_an_svg() {
        for name in ["square-terminal", "x", "plus", "maximize-2", "loader-circle", "panel-left", "trash-2"] {
            assert!(super::svg(name).starts_with("<svg"), "{name}");
        }
        assert_eq!(super::svg("no-such-icon"), "");
    }
}
