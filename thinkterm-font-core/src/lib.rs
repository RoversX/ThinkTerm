//! Font types and traits shared by every ThinkTerm renderer. See
//! Cargo.toml for the charter.

pub mod glyph;
pub mod metrics;
pub mod traits;
pub mod units;

pub use glyph::{FallbackIdx, GlyphInfo, GlyphInfoParts, RasterizedGlyph};
pub use metrics::FontMetrics;
pub use traits::{FontRasterizer, FontShaper, PresentationWidth};
pub use wezterm_bidi::Direction;
