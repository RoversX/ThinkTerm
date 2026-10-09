use luahelper::impl_lua_conversion_dynamic;
use wezterm_dynamic::{FromDynamic, ToDynamic};

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum FrontEndSelection {
    /// The default on Windows. It falls back to WebGpu the way WebGpu falls
    /// back to it, so a machine without a usable OpenGL still opens.
    OpenGL,
    /// The default elsewhere since the main window learned to fall back: wgpu
    /// asks for every backend it has (Metal, then DX12/Vulkan/GL depending on
    /// the platform; on Windows GL is left out where OpenGL would be Mesa),
    /// and if none of them can be brought up the window quietly opens on
    /// OpenGL instead of failing to open at all. Software never falls back.
    WebGpu,
    Software,
}

impl Default for FrontEndSelection {
    fn default() -> Self {
        if cfg!(windows) {
            Self::OpenGL
        } else {
            Self::WebGpu
        }
    }
}

/// Corresponds to <https://docs.rs/wgpu/latest/wgpu/struct.AdapterInfo.html>
#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct GpuInfo {
    pub name: String,
    pub device_type: String,
    pub backend: String,
    pub driver: Option<String>,
    pub driver_info: Option<String>,
    pub vendor: Option<u32>,
    pub device: Option<u32>,
}
impl_lua_conversion_dynamic!(GpuInfo);

impl ToString for GpuInfo {
    fn to_string(&self) -> String {
        let mut result = format!(
            "name={}, device_type={}, backend={}",
            self.name, self.device_type, self.backend
        );
        if let Some(driver) = &self.driver {
            result.push_str(&format!(", driver={driver}"));
        }
        if let Some(driver_info) = &self.driver_info {
            result.push_str(&format!(", driver_info={driver_info}"));
        }
        if let Some(vendor) = &self.vendor {
            result.push_str(&format!(", vendor={vendor}"));
        }
        if let Some(device) = &self.device {
            result.push_str(&format!(", device={device}"));
        }
        result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromDynamic, ToDynamic)]
pub enum WebGpuPowerPreference {
    LowPower,
    HighPerformance,
}

impl Default for WebGpuPowerPreference {
    fn default() -> Self {
        Self::LowPower
    }
}
