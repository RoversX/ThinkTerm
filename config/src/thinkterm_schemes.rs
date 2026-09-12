//! Colour schemes ThinkTerm ships itself.
//!
//! Separate from `scheme_data.rs`, which `sync-color-schemes` regenerates from
//! upstream and would drop anything added by hand.

/// The light half of `Apple System Colors`.
///
/// The dark one upstream ships is built from macOS's *dark appearance* system
/// colours -- its bright row is `systemRed`, `systemGreen`, `systemBlue` and
/// friends verbatim. This is the same construction on the other side: the
/// bright row is the *light appearance* values, and the normal row is each of
/// them walked down in OkLab until it clears 4.5:1 on white, which is what a
/// light ground needs and what the vivid values do not give (`#ffcc00` on
/// white is 1.5:1). Hue and chroma are untouched, so a yellow still reads as
/// that yellow.
const APPLE_SYSTEM_COLORS_LIGHT: &str = r##"[colors]
ansi = [
    "#000000",
    "#e9281f",
    "#008a00",
    "#9d6d00",
    "#0070f4",
    "#a84bd7",
    "#007fae",
    "#c7c7cc",
]
background = "#ffffff"
brights = [
    "#8e8e93",
    "#ff3b30",
    "#28cd41",
    "#ffcc00",
    "#007aff",
    "#af52de",
    "#55bef0",
    "#ffffff",
]
cursor_bg = "#8e8e93"
cursor_border = "#8e8e93"
cursor_fg = "#ffffff"
foreground = "#000000"
selection_bg = "#b5d5ff"
selection_fg = "#000000"

[colors.indexed]

[metadata]
aliases = []
name = "Apple System Colors (Light)"
"##;

pub const SCHEMES: [(&'static str, &'static str); 1] = [(
    "Apple System Colors (Light)",
    APPLE_SYSTEM_COLORS_LIGHT,
)];
