use crate::parser::ParsedFont;

pub mod harfbuzz;
pub use thinkterm_font_core::{
    FallbackIdx, FontMetrics, FontShaper, GlyphInfo, GlyphInfoParts, PresentationWidth,
};
pub use wezterm_bidi::Direction;

pub use config::FontShaperSelection;

pub fn new_shaper(
    config: &config::ConfigHandle,
    handles: &[ParsedFont],
) -> anyhow::Result<Box<dyn FontShaper>> {
    Ok(new_native_shaper(config, handles)?)
}

pub(crate) fn new_native_shaper(
    config: &config::ConfigHandle,
    handles: &[ParsedFont],
) -> anyhow::Result<Box<harfbuzz::HarfbuzzShaper>> {
    match config.font_shaper {
        FontShaperSelection::Harfbuzz => {
            Ok(Box::new(harfbuzz::HarfbuzzShaper::new(config, handles)?))
        }
        FontShaperSelection::Allsorts => {
            anyhow::bail!("The incomplete Allsorts shaper has been removed");
        }
    }
}
