//! The GPU-backend-neutral terminal renderer core. See Cargo.toml for the
//! charter. The desktop (`wezterm-gui`, through `window`) and the browser
//! client both build their frames out of these pieces.

pub mod atlas;
pub mod bitmaps;
pub mod customglyph;
pub mod geom;
#[cfg(feature = "wgpu")]
pub mod pipeline;
pub mod quad;
pub mod vertex;

pub use geom::Dimensions;
pub use wezterm_color_types::HsbTransform;

/// The one shader every ThinkTerm renderer draws with. Kept as source so
/// each backend compiles it for its own device.
pub const SHADER_SOURCE: &str = include_str!("shader.wgsl");

#[cfg(test)]
mod shader_tests {
    #[test]
    fn main_wgsl_shader_parses_and_validates() {
        let module = naga::front::wgsl::parse_str(super::SHADER_SOURCE)
            .expect("main WGSL shader should parse");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("main WGSL shader should validate");
    }
}
