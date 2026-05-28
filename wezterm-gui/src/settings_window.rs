use crate::customglyph::{BlockKey, Poly};
use crate::quad::{QuadTrait, TripleLayerQuadAllocator, TripleLayerQuadAllocatorTrait};
use crate::renderstate::{RenderContext, RenderState};
use crate::termwindow::render::corners::{
    BOTTOM_LEFT_ROUNDED_CORNER, BOTTOM_RIGHT_ROUNDED_CORNER, TOP_LEFT_ROUNDED_CORNER,
    TOP_RIGHT_ROUNDED_CORNER,
};
use crate::termwindow::render::draw::draw_webgpu_layers;
use crate::termwindow::webgpu::WebGpuState;
use crate::ui::{
    rect, ButtonSpec, ControlState, InteractionState, ResizablePaneState, ScrollState,
    ScrollbarSpec, SettingsIcon, SvgIcon, TextInputSpec, TextInputState, UiContext, UiPalette,
    UiTokens, WidgetKind,
};
use crate::utilsprites::RenderMetrics;
use anyhow::Context;
use config::{configuration, Dimension, GeometryOrigin};
use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wezterm_bidi::Direction;
use wezterm_font::{FontConfiguration, LoadedFont};
use window::bitmaps::atlas::OutOfTextureSpace;
use window::color::LinearRgba;
use window::{
    Appearance, Clipboard, Connection, ConnectionOps, Dimensions, KeyCode, KeyEvent, Modifiers,
    MouseButtons, MouseCursor, MouseEvent, MouseEventKind, MousePress, RequestedWindowGeometry,
    Window, WindowEvent, WindowOps,
};

use crate::native_settings::{
    NativeAppIcon, NativeThemeMode, ThinkTermNativeSettings, DEFAULT_PANE_HEADER_FONT_SIZE,
    DEFAULT_SETTINGS_FONT_SIZE, DEFAULT_SIDEBAR_FONT_SIZE, DEFAULT_TAB_FONT_SIZE,
};

const DEFAULT_WIDTH: usize = 1840;
const DEFAULT_HEIGHT: usize = 1205;
const SIDEBAR_BRAND_FONT_SIZE: f64 = 17.0;
const SIDEBAR_BRAND_FONT_WEIGHT: u16 = 750;
const CONTROL_HEIGHT: f32 = 56.0;
const CONTROL_RADIUS: f32 = 14.0;
const NAV_ROW_RADIUS: f32 = 14.0;
const NAV_ROW_HEIGHT: f32 = 56.0;
const NAV_ROW_STEP: f32 = 68.0;
const HEADER_HEIGHT: f32 = 132.0;
const SIDEBAR_TITLE_Y: f32 = 78.0;
const SIDEBAR_SEARCH_Y: f32 = 142.0;
const SIDEBAR_LIST_TOP: f32 = 222.0;
const CONTENT_TITLE_Y: f32 = 82.0;
const CONTENT_SECTION_Y: f32 = 168.0;
const CONTENT_RULE_Y: f32 = 202.0;

thread_local! {
    static SETTINGS_WINDOW: RefCell<Option<Rc<RefCell<SettingsWindow>>>> = RefCell::new(None);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsSection {
    General,
    Appearance,
    Terminal,
    Workspaces,
    Keymap,
    Compatibility,
    Developer,
    UiKit,
    Memory,
    About,
}

const BASE_SECTIONS: &[SettingsSection] = &[
    SettingsSection::General,
    SettingsSection::Appearance,
    SettingsSection::Terminal,
    SettingsSection::Workspaces,
    SettingsSection::Keymap,
    SettingsSection::Compatibility,
    SettingsSection::Developer,
    SettingsSection::About,
];

const DEVELOPER_SECTIONS: &[SettingsSection] = &[SettingsSection::UiKit, SettingsSection::Memory];

impl SettingsSection {
    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Appearance => "Appearance",
            Self::Terminal => "Terminal",
            Self::Workspaces => "Workspaces",
            Self::Keymap => "Keymap",
            Self::Compatibility => "Compatibility",
            Self::Developer => "Developer",
            Self::UiKit => "UI Kit",
            Self::Memory => "Memory",
            Self::About => "About",
        }
    }

    fn icon(self) -> SettingsIcon {
        match self {
            Self::General => SettingsIcon::General,
            Self::Appearance => SettingsIcon::Appearance,
            Self::Terminal => SettingsIcon::Terminal,
            Self::Workspaces => SettingsIcon::Workspaces,
            Self::Keymap => SettingsIcon::Keymap,
            Self::Compatibility => SettingsIcon::Sync,
            Self::Developer => SettingsIcon::Developer,
            Self::UiKit => SettingsIcon::UiKit,
            Self::Memory => SettingsIcon::Memory,
            Self::About => SettingsIcon::About,
        }
    }

    fn search_terms(self) -> &'static [&'static str] {
        match self {
            Self::General => &[
                "Config Source",
                "ThinkTerm Native Settings",
                "Theme Mode",
                "Native Settings",
                "Restore Main Window Frame",
                "Window Size",
                "Window Position",
                "Configuration",
            ],
            Self::Appearance => &[
                "Theme Mode",
                "Effective Color Scheme",
                "Config Source",
                "App Icon",
                "Settings UI Font Size",
                "Workspace Sidebar Font Size",
                "Tab Bar Font Size",
                "Pane Header Font Size",
                "Settings UI Font Weight",
                "Typography",
                "Theme",
                "Font",
                "Weight",
            ],
            Self::Terminal => &[
                "Font Size",
                "Font Family",
                "ThinkTerm Font Size",
                "Native Font Family",
                "Terminal",
            ],
            Self::Workspaces => &["Workspace", "Sidebar", "Session", "Layout"],
            Self::Keymap => &["Keymap", "Keyboard", "Shortcut", "Command Palette"],
            Self::Compatibility => &[
                "Active Source",
                "GUI Editing",
                "Open Config File",
                "Compatibility Status",
                "WezTerm",
                "Import",
            ],
            Self::Developer => &[
                "Developer Mode",
                "Diagnostics",
                "Debug Pages",
                "Memory Diagnostics",
                "UI Kit",
            ],
            Self::UiKit => &[
                "Search Field",
                "Sidebar Rows",
                "Buttons and Controls",
                "Setting Row",
                "Palette",
                "Layout",
                "Typography",
                "Component Preview",
                "Component Styles",
            ],
            Self::Memory => &[
                "Memory",
                "Diagnostics",
                "Manual Sampling",
                "Copy",
                "Refresh",
                "Physical Footprint",
                "RSS",
                "vmmap",
                "IOAccelerator",
                "IOSurface",
                "Graphics",
            ],
            Self::About => &["About", "Version", "ThinkTerm"],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsAction {
    Select(SettingsSection),
    OpenConfigFile,
    ShowCompatibilityStatus,
    ToggleMainWindowFrameRestore,
    ToggleDeveloperMode,
    ToggleMemoryMonitoring,
    RefreshMemorySnapshot,
    CopyMemorySnapshot,
    ToggleThemeModeMenu,
    SetThemeMode(NativeThemeMode),
    ToggleAppIconMenu,
    SetAppIcon(NativeAppIcon),
    SearchInput,
    DecreaseFontSize,
    IncreaseFontSize,
    ResetFontSize,
    DecreaseChromeFontSize(ChromeFontArea),
    IncreaseChromeFontSize(ChromeFontArea),
    ResetChromeFontSize(ChromeFontArea),
    DecreaseSettingsFontWeight,
    IncreaseSettingsFontWeight,
    ResetSettingsFontWeight,
    FontFamilyInput,
    ClearSearch,
    SidebarResize,
    SidebarScrollArea,
    ContentScrollArea,
}

#[derive(Debug, Clone, Copy)]
enum SettingsDrag {
    SidebarResize { start_x: f32, start_width: f32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsDropdown {
    ThemeMode,
    AppIcon,
}

#[derive(Debug, Clone)]
struct MemorySnapshot {
    captured_at: Instant,
    pid: u32,
    resident_size: Option<u64>,
    physical_footprint: Option<u64>,
    peak_physical_footprint: Option<u64>,
    vmmap_total_resident: Option<u64>,
    vmmap_graphics_resident: Option<u64>,
    vmmap_malloc_resident: Option<u64>,
    vmmap_text_resident: Option<u64>,
    vmmap_iosurface_resident: Option<u64>,
    vmmap_error: Option<String>,
    error: Option<String>,
}

impl MemorySnapshot {
    fn has_vmmap_breakdown(&self) -> bool {
        self.vmmap_total_resident.is_some()
            || self.vmmap_graphics_resident.is_some()
            || self.vmmap_malloc_resident.is_some()
            || self.vmmap_text_resident.is_some()
            || self.vmmap_iosurface_resident.is_some()
    }

    fn log_line(&self) -> String {
        format!(
            "pid={} rss={} physical_footprint={} peak_physical_footprint={} vmmap_total_resident={} graphics_resident={} malloc_resident={}{}{}",
            self.pid,
            self.resident_size
                .map(format_bytes)
                .unwrap_or_else(|| "unavailable".to_string()),
            self.physical_footprint
                .map(format_bytes)
                .unwrap_or_else(|| "unavailable".to_string()),
            self.peak_physical_footprint
                .map(format_bytes)
                .unwrap_or_else(|| "unavailable".to_string()),
            self.vmmap_total_resident
                .map(format_bytes)
                .unwrap_or_else(|| "unavailable".to_string()),
            self.vmmap_graphics_resident
                .map(format_bytes)
                .unwrap_or_else(|| "unavailable".to_string()),
            self.vmmap_malloc_resident
                .map(format_bytes)
                .unwrap_or_else(|| "unavailable".to_string()),
            self.vmmap_error
                .as_ref()
                .map(|error| format!(" vmmap_error={error}"))
                .unwrap_or_default(),
            self.error
                .as_ref()
                .map(|error| format!(" error={error}"))
                .unwrap_or_default()
        )
    }

    fn summary_for_clipboard(&self) -> String {
        let mut lines = vec![
            "ThinkTerm Memory Snapshot".to_string(),
            format!("pid: {}", self.pid),
            format!(
                "rss: {}",
                self.resident_size
                    .map(format_bytes)
                    .unwrap_or_else(|| "unavailable".to_string())
            ),
            format!(
                "physical_footprint: {}",
                self.physical_footprint
                    .map(format_bytes)
                    .unwrap_or_else(|| "unavailable".to_string())
            ),
            format!(
                "peak_physical_footprint: {}",
                self.peak_physical_footprint
                    .map(format_bytes)
                    .unwrap_or_else(|| "unavailable".to_string())
            ),
            format!(
                "vmmap_total_resident: {}",
                self.vmmap_total_resident
                    .map(format_bytes)
                    .unwrap_or_else(|| "not captured".to_string())
            ),
            format!(
                "graphics_resident: {}",
                self.vmmap_graphics_resident
                    .map(format_bytes)
                    .unwrap_or_else(|| "not captured".to_string())
            ),
            format!(
                "iosurface_resident: {}",
                self.vmmap_iosurface_resident
                    .map(format_bytes)
                    .unwrap_or_else(|| "not captured".to_string())
            ),
            format!(
                "malloc_resident: {}",
                self.vmmap_malloc_resident
                    .map(format_bytes)
                    .unwrap_or_else(|| "not captured".to_string())
            ),
            format!(
                "text_segments_resident: {}",
                self.vmmap_text_resident
                    .map(format_bytes)
                    .unwrap_or_else(|| "not captured".to_string())
            ),
        ];
        if let Some(error) = &self.vmmap_error {
            lines.push(format!("vmmap_error: {error}"));
        }
        if let Some(error) = &self.error {
            lines.push(format!("error: {error}"));
        }
        lines.join("\n")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChromeFontArea {
    Settings,
    Sidebar,
    TabBar,
    PaneHeader,
}

impl ChromeFontArea {
    fn label(self) -> &'static str {
        match self {
            Self::Settings => "Settings UI Font Size",
            Self::Sidebar => "Workspace Sidebar Font Size",
            Self::TabBar => "Tab Bar Font Size",
            Self::PaneHeader => "Pane Header Font Size",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Settings => "Controls the Settings window chrome and content text.",
            Self::Sidebar => "Saved separately for the main workspace sidebar.",
            Self::TabBar => "Saved separately for the top terminal tab bar.",
            Self::PaneHeader => "Saved separately for split-pane header labels.",
        }
    }

    fn default_size(self) -> f64 {
        match self {
            Self::Settings => DEFAULT_SETTINGS_FONT_SIZE,
            Self::Sidebar => DEFAULT_SIDEBAR_FONT_SIZE,
            Self::TabBar => DEFAULT_TAB_FONT_SIZE,
            Self::PaneHeader => DEFAULT_PANE_HEADER_FONT_SIZE,
        }
    }
}

#[derive(Debug, Clone)]
struct SettingsUiState {
    tokens: UiTokens,
    sidebar: ResizablePaneState,
    sidebar_scroll: ScrollState,
    content_scroll: ScrollState,
    search: TextInputState,
    font_size_input: TextInputState,
    font_family_input: TextInputState,
    interaction: InteractionState<SettingsAction>,
    drag: Option<SettingsDrag>,
    open_dropdown: Option<SettingsDropdown>,
    memory_monitoring: bool,
    memory_monitor_generation: u64,
    memory_snapshot: Option<MemorySnapshot>,
    main_window_resource_lines: Vec<String>,
    memory_snapshot_copied_until: Option<Instant>,
    sidebar_scrollbar_visible_until: Option<Instant>,
    content_scrollbar_visible_until: Option<Instant>,
}

impl SettingsUiState {
    fn new() -> Self {
        let tokens = UiTokens::default();
        Self {
            sidebar: ResizablePaneState::new(
                tokens.sidebar_default_width,
                tokens.sidebar_min_width,
                tokens.sidebar_max_width,
            ),
            tokens,
            sidebar_scroll: ScrollState::new(),
            content_scroll: ScrollState::new(),
            search: TextInputState::new(),
            font_size_input: TextInputState::new(),
            font_family_input: TextInputState::new(),
            interaction: InteractionState::default(),
            drag: None,
            open_dropdown: None,
            memory_monitoring: false,
            memory_monitor_generation: 0,
            memory_snapshot: None,
            main_window_resource_lines: Vec::new(),
            memory_snapshot_copied_until: None,
            sidebar_scrollbar_visible_until: None,
            content_scrollbar_visible_until: None,
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    let mib = bytes as f64 / 1024.0 / 1024.0;
    if mib >= 1024.0 {
        format!("{:.2} GB", mib / 1024.0)
    } else {
        format!("{mib:.1} MB")
    }
}

fn capture_memory_snapshot(detailed: bool) -> MemorySnapshot {
    let pid = std::process::id();
    let mut snapshot = MemorySnapshot {
        captured_at: Instant::now(),
        pid,
        resident_size: None,
        physical_footprint: None,
        peak_physical_footprint: None,
        vmmap_total_resident: None,
        vmmap_graphics_resident: None,
        vmmap_malloc_resident: None,
        vmmap_text_resident: None,
        vmmap_iosurface_resident: None,
        vmmap_error: None,
        error: None,
    };

    match capture_process_memory_info(pid) {
        Ok(info) => {
            snapshot.resident_size = Some(info.resident_size);
            snapshot.physical_footprint = Some(info.physical_footprint);
            snapshot.peak_physical_footprint = Some(info.peak_physical_footprint);
        }
        Err(err) => {
            snapshot.error = Some(err);
        }
    }

    if detailed {
        match capture_vmmap_breakdown(pid) {
            Ok(breakdown) => {
                snapshot.vmmap_total_resident = breakdown.total_resident;
                snapshot.vmmap_graphics_resident = breakdown.graphics_resident;
                snapshot.vmmap_malloc_resident = breakdown.malloc_resident;
                snapshot.vmmap_text_resident = breakdown.text_resident;
                snapshot.vmmap_iosurface_resident = breakdown.iosurface_resident;
            }
            Err(err) => {
                snapshot.vmmap_error = Some(err);
            }
        }
    }

    snapshot
}

#[derive(Debug, Clone, Default)]
struct VmmapBreakdown {
    total_resident: Option<u64>,
    graphics_resident: Option<u64>,
    malloc_resident: Option<u64>,
    text_resident: Option<u64>,
    iosurface_resident: Option<u64>,
}

fn capture_vmmap_breakdown(pid: u32) -> Result<VmmapBreakdown, String> {
    let output = Command::new("/usr/bin/vmmap")
        .arg("-summary")
        .arg(pid.to_string())
        .output()
        .map_err(|err| format!("failed to run vmmap: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().to_string());
    }

    parse_vmmap_summary(&String::from_utf8_lossy(&output.stdout))
}

fn parse_vmmap_summary(summary: &str) -> Result<VmmapBreakdown, String> {
    let mut result = VmmapBreakdown::default();
    let mut graphics_resident = 0;
    let mut has_graphics = false;
    let mut malloc_resident = 0;
    let mut has_malloc = false;
    let mut in_region_table = false;

    for line in summary.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("REGION TYPE") {
            in_region_table = true;
            continue;
        }
        if !in_region_table || trimmed.is_empty() || trimmed.starts_with("==========") {
            continue;
        }

        let Some((name, values)) = parse_vmmap_region_line(line) else {
            continue;
        };
        if values.len() < 2 {
            continue;
        }
        let resident = values[1];
        let name = name.trim();

        if name == "TOTAL" {
            result.total_resident = Some(resident);
            break;
        } else if name == "__TEXT" {
            result.text_resident = Some(resident);
        } else if name == "IOSurface" {
            result.iosurface_resident = Some(resident);
            graphics_resident += resident;
            has_graphics = true;
        } else if name == "IOAccelerator (graphics)" || name == "owned unmapped (graphics)" {
            graphics_resident += resident;
            has_graphics = true;
        } else if name.starts_with("MALLOC") {
            malloc_resident += resident;
            has_malloc = true;
        }
    }

    if has_graphics {
        result.graphics_resident = Some(graphics_resident);
    }
    if has_malloc {
        result.malloc_resident = Some(malloc_resident);
    }
    Ok(result)
}

fn parse_vmmap_region_line(line: &str) -> Option<(String, Vec<u64>)> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let first_size = tokens
        .iter()
        .position(|token| parse_vmmap_size(token).is_some())?;
    if first_size == 0 {
        return None;
    }
    let name = tokens[..first_size].join(" ");
    let values = tokens[first_size..]
        .iter()
        .filter_map(|token| parse_vmmap_size(token))
        .collect::<Vec<_>>();
    Some((name, values))
}

fn parse_vmmap_size(token: &str) -> Option<u64> {
    let token = token.trim_end_matches(',');
    let (number, multiplier) = if let Some(number) = token.strip_suffix('K') {
        (number, 1024.0)
    } else if let Some(number) = token.strip_suffix('M') {
        (number, 1024.0 * 1024.0)
    } else if let Some(number) = token.strip_suffix('G') {
        (number, 1024.0 * 1024.0 * 1024.0)
    } else {
        return None;
    };
    number
        .parse::<f64>()
        .ok()
        .map(|value| (value * multiplier).round() as u64)
}

#[cfg(test)]
mod memory_parser_tests {
    use super::*;

    #[test]
    fn vmmap_summary_uses_region_type_table_total() {
        let summary = r#"
ReadOnly portion of Libraries: Total=579.0M resident=421.4M(73%) swapped_out_or_unallocated=157.6M(27%)
Writable regions: Total=81.0M written=19.4M(24%) resident=19.4M(24%) swapped_out=0K(0%) unallocated=61.6M(76%)

                                VIRTUAL RESIDENT    DIRTY  SWAPPED VOLATILE   NONVOL    EMPTY   REGION
REGION TYPE                        SIZE     SIZE     SIZE     SIZE     SIZE     SIZE     SIZE    COUNT (non-coalesced)
===========                     ======= ========    =====  ======= ========   ======    =====  =======
MALLOC metadata                    752K     192K     192K       0K       0K       0K       0K        4
IOSurface                         96.0M    82.6M      64K       0K       0K       0K       0K        2
IOAccelerator (graphics)          64.0M    36.2M      16K       0K       0K       0K       0K        1
__TEXT                           430.0M   421.4M       0K       0K       0K       0K       0K       46
===========                     ======= ========    =====  ======= ========   ======    =====  =======
TOTAL                            802.4M   563.7M    19.8M       0K       0K       0K       0K      261

MALLOC ZONE                         SIZE       SIZE       SIZE       SIZE      COUNT  ALLOCATED  FRAG SIZE  % FRAG   COUNT
===========                      =======  =========  =========  =========  =========  =========  =========  ======  ======
TOTAL                              94.4M      19.3M      19.3M         0K        184        11K       245K     96%       5
"#;

        let parsed = parse_vmmap_summary(summary).unwrap();
        assert_eq!(parsed.total_resident, parse_vmmap_size("563.7M"));
        assert_eq!(parsed.iosurface_resident, parse_vmmap_size("82.6M"));
        assert_eq!(
            parsed.graphics_resident,
            Some(parse_vmmap_size("82.6M").unwrap() + parse_vmmap_size("36.2M").unwrap())
        );
        assert_eq!(parsed.malloc_resident, Some(192 * 1024));
        assert_eq!(parsed.text_resident, parse_vmmap_size("421.4M"));
    }
}

#[derive(Debug, Clone, Copy)]
struct ProcessMemoryInfo {
    resident_size: u64,
    physical_footprint: u64,
    peak_physical_footprint: u64,
}

#[cfg(target_os = "macos")]
fn capture_process_memory_info(pid: u32) -> Result<ProcessMemoryInfo, String> {
    const RUSAGE_INFO_V4: libc::c_int = 4;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RUsageInfoV4 {
        ri_uuid: [u8; 16],
        ri_user_time: u64,
        ri_system_time: u64,
        ri_pkg_idle_wkups: u64,
        ri_interrupt_wkups: u64,
        ri_pageins: u64,
        ri_wired_size: u64,
        ri_resident_size: u64,
        ri_phys_footprint: u64,
        ri_proc_start_abstime: u64,
        ri_proc_exit_abstime: u64,
        ri_child_user_time: u64,
        ri_child_system_time: u64,
        ri_child_pkg_idle_wkups: u64,
        ri_child_interrupt_wkups: u64,
        ri_child_pageins: u64,
        ri_child_elapsed_abstime: u64,
        ri_diskio_bytesread: u64,
        ri_diskio_byteswritten: u64,
        ri_cpu_time_qos_default: u64,
        ri_cpu_time_qos_maintenance: u64,
        ri_cpu_time_qos_background: u64,
        ri_cpu_time_qos_utility: u64,
        ri_cpu_time_qos_legacy: u64,
        ri_cpu_time_qos_user_initiated: u64,
        ri_cpu_time_qos_user_interactive: u64,
        ri_billed_system_time: u64,
        ri_serviced_system_time: u64,
        ri_logical_writes: u64,
        ri_lifetime_max_phys_footprint: u64,
        ri_instructions: u64,
        ri_cycles: u64,
        ri_billed_energy: u64,
        ri_serviced_energy: u64,
    }

    unsafe extern "C" {
        fn proc_pid_rusage(
            pid: libc::c_int,
            flavor: libc::c_int,
            buffer: *mut libc::c_void,
        ) -> libc::c_int;
    }

    let mut info = std::mem::MaybeUninit::<RUsageInfoV4>::zeroed();
    let result = unsafe {
        proc_pid_rusage(
            pid as libc::c_int,
            RUSAGE_INFO_V4,
            info.as_mut_ptr().cast::<libc::c_void>(),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }

    let info = unsafe { info.assume_init() };
    Ok(ProcessMemoryInfo {
        resident_size: info.ri_resident_size,
        physical_footprint: info.ri_phys_footprint,
        peak_physical_footprint: info.ri_lifetime_max_phys_footprint,
    })
}

#[cfg(not(target_os = "macos"))]
fn capture_process_memory_info(_pid: u32) -> Result<ProcessMemoryInfo, String> {
    Err("memory diagnostics are only wired on macOS for now".to_string())
}

struct StyleToken<'a> {
    name: &'a str,
    value: &'a str,
    swatch: Option<LinearRgba>,
}

#[derive(Clone, Copy)]
struct SettingsPalette {
    window_bg: LinearRgba,
    sidebar_bg: LinearRgba,
    separator: LinearRgba,
    search_bg: LinearRgba,
    search_border: LinearRgba,
    nav_hover_bg: LinearRgba,
    nav_pressed_bg: LinearRgba,
    nav_selected_bg: LinearRgba,
    control_bg: LinearRgba,
    control_hover_bg: LinearRgba,
    control_pressed_bg: LinearRgba,
    control_border: LinearRgba,
    card_bg: LinearRgba,
    title: LinearRgba,
    text: LinearRgba,
    secondary_text: LinearRgba,
    muted_text: LinearRgba,
    selected_text: LinearRgba,
    rule: LinearRgba,
}

pub fn show() {
    let already_open = SETTINGS_WINDOW.with(|slot| {
        if let Some(settings) = slot.borrow().as_ref() {
            if let Some(window) = settings.borrow().window.as_ref() {
                window.show();
                window.focus();
                return true;
            }
        }
        false
    });

    if already_open {
        return;
    }

    promise::spawn::spawn(async {
        if let Err(err) = SettingsWindow::open().await {
            log::error!("failed to open settings window: {err:#}");
        }
    })
    .detach();
}

struct SettingsWindow {
    window: Option<Window>,
    dimensions: Dimensions,
    fonts: Rc<FontConfiguration>,
    ui_font: Rc<LoadedFont>,
    title_font: Rc<LoadedFont>,
    sidebar_title_font: Rc<LoadedFont>,
    metrics: RenderMetrics,
    render_state: Option<RenderState>,
    webgpu: Option<Rc<WebGpuState>>,
    appearance: Appearance,
    selected: SettingsSection,
    native_settings: ThinkTermNativeSettings,
    ui: SettingsUiState,
    ui_context: UiContext<SettingsAction>,
    status: String,
}

impl SettingsWindow {
    async fn open() -> anyhow::Result<()> {
        let config = configuration();
        let dpi = window::default_dpi() as usize;
        let fonts = Rc::new(FontConfiguration::new(Some(config.clone()), dpi)?);
        let native_settings = Self::load_native_settings();
        let settings_font_size = crate::native_settings::settings_font_size(&native_settings);
        let settings_font_weight = crate::native_settings::settings_font_weight(&native_settings);
        let title_font = fonts
            .title_font_with_size_and_weight(settings_font_size + 4.0, settings_font_weight)?;
        let sidebar_title_font = fonts
            .title_font_with_size_and_weight(SIDEBAR_BRAND_FONT_SIZE, SIDEBAR_BRAND_FONT_WEIGHT)?;
        let ui_font = fonts
            .command_palette_font_with_size_and_weight(settings_font_size, settings_font_weight)?;
        let metrics = RenderMetrics::with_font_metrics(&ui_font.metrics());
        let appearance = Connection::get()
            .map(|conn| conn.get_appearance())
            .unwrap_or(Appearance::Dark);
        let dimensions = Dimensions {
            pixel_width: DEFAULT_WIDTH,
            pixel_height: DEFAULT_HEIGHT,
            dpi,
        };

        let mut ui = SettingsUiState::new();
        ui.font_size_input.text = native_settings
            .terminal
            .font_size
            .map(|value| format!("{value:.1}"))
            .unwrap_or_else(|| format!("{:.1}", config.font_size));
        ui.font_family_input.text = native_settings
            .terminal
            .font_family
            .clone()
            .unwrap_or_else(|| Self::effective_font_family(&config));

        let settings = Rc::new(RefCell::new(Self {
            window: None,
            dimensions,
            fonts: Rc::clone(&fonts),
            ui_font,
            title_font,
            sidebar_title_font,
            metrics,
            render_state: None,
            webgpu: None,
            appearance,
            selected: SettingsSection::Appearance,
            native_settings,
            ui,
            ui_context: UiContext::default(),
            status: Self::initial_status(),
        }));

        let event_settings = Rc::clone(&settings);
        let geometry = RequestedWindowGeometry {
            width: Dimension::Pixels(DEFAULT_WIDTH as f32),
            height: Dimension::Pixels(DEFAULT_HEIGHT as f32),
            x: None,
            y: None,
            macos_frame_autosave_name: None,
            origin: GeometryOrigin::default(),
        };

        let window = Window::new_window(
            "thinkterm-settings",
            "ThinkTerm Settings",
            geometry,
            Some(&config),
            Rc::clone(&fonts),
            move |event, window| {
                if let Err(err) = event_settings.borrow_mut().dispatch(event, window) {
                    log::error!("settings window event failed: {err:#}");
                }
            },
        )
        .await?;

        window.set_title("ThinkTerm Settings");
        let webgpu = Rc::new(WebGpuState::new(&window, dimensions, &config).await?);
        {
            let mut settings = settings.borrow_mut();
            settings.created(RenderContext::WebGpu(Rc::clone(&webgpu)))?;
            settings.webgpu.replace(webgpu);
            settings.window.replace(window.clone());
        }

        SETTINGS_WINDOW.with(|slot| slot.replace(Some(settings)));

        window.show();
        window.invalidate();

        Ok(())
    }

    fn created(&mut self, context: RenderContext) -> anyhow::Result<()> {
        self.render_state
            .replace(RenderState::new(context, &self.fonts, &self.metrics, 256)?);
        Ok(())
    }

    fn dispatch(&mut self, event: WindowEvent, window: &Window) -> anyhow::Result<bool> {
        match event {
            WindowEvent::CloseRequested => {
                window.close();
                Ok(true)
            }
            WindowEvent::Destroyed => {
                self.ui.memory_monitoring = false;
                self.ui.memory_monitor_generation =
                    self.ui.memory_monitor_generation.wrapping_add(1);
                self.render_state.take();
                self.webgpu.take();
                self.window.take();
                SETTINGS_WINDOW.with(|slot| {
                    slot.borrow_mut().take();
                });
                Ok(true)
            }
            WindowEvent::Resized { dimensions, .. } => {
                self.dimensions = dimensions;
                if let Some(webgpu) = self.webgpu.as_ref() {
                    webgpu.resize(dimensions);
                }
                self.clamp_sidebar_to_window();
                window.invalidate();
                Ok(true)
            }
            WindowEvent::NeedRepaint => Ok(self.do_paint(window)),
            WindowEvent::MouseEvent(event) => {
                self.mouse_event(event, window);
                Ok(true)
            }
            WindowEvent::KeyEvent(event) => {
                if self.key_event(event, window) {
                    window.invalidate();
                }
                Ok(true)
            }
            WindowEvent::MouseLeave => {
                self.ui.interaction.hovered = None;
                self.ui.interaction.pressed = None;
                self.ui.drag = None;
                window.set_cursor(Some(MouseCursor::Arrow));
                window.invalidate();
                Ok(true)
            }
            WindowEvent::AppearanceChanged(appearance) => {
                self.appearance = appearance;
                window.invalidate();
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    fn mouse_event(&mut self, event: MouseEvent, window: &Window) {
        let x = event.coords.x as f32;
        let y = event.coords.y as f32;
        let hit = self.action_at(x, y);
        let action = hit.map(|target| target.action);

        match event.kind {
            MouseEventKind::Move => {
                if let Some(SettingsDrag::SidebarResize {
                    start_x,
                    start_width,
                }) = self.ui.drag
                {
                    self.ui.sidebar.set_width(start_width + x - start_x);
                    window.invalidate();
                    return;
                }

                if self.ui.interaction.hovered != action {
                    self.ui.interaction.hovered = action;
                    window.set_cursor(Some(match hit.map(|target| target.kind) {
                        Some(WidgetKind::ResizeHandle) => MouseCursor::SizeLeftRight,
                        Some(WidgetKind::TextInput) => MouseCursor::Text,
                        Some(
                            WidgetKind::Button
                            | WidgetKind::SidebarRow
                            | WidgetKind::PreviewControl,
                        ) => MouseCursor::Hand,
                        Some(WidgetKind::ScrollArea) | None => MouseCursor::Arrow,
                    }));
                    window.invalidate();
                }
            }
            MouseEventKind::Press(MousePress::Left) => {
                self.ui.interaction.hovered = action;
                self.ui.interaction.pressed = action;
                match action {
                    Some(SettingsAction::SearchInput | SettingsAction::FontFamilyInput) => {
                        self.ui.interaction.focused = action;
                        self.ui.open_dropdown = None;
                    }
                    Some(SettingsAction::SidebarResize) => {
                        self.ui.drag = Some(SettingsDrag::SidebarResize {
                            start_x: x,
                            start_width: self.ui.sidebar.width,
                        });
                        self.ui.open_dropdown = None;
                    }
                    Some(
                        SettingsAction::ToggleThemeModeMenu
                        | SettingsAction::SetThemeMode(_)
                        | SettingsAction::ToggleAppIconMenu
                        | SettingsAction::SetAppIcon(_),
                    ) => {
                        self.ui.interaction.focused = None;
                    }
                    Some(_) => {
                        self.ui.interaction.focused = None;
                        self.ui.open_dropdown = None;
                    }
                    None => {
                        self.ui.interaction.focused = None;
                        self.ui.open_dropdown = None;
                    }
                }
                window.invalidate();
            }
            MouseEventKind::Release(MousePress::Left) => {
                let pressed = self.ui.interaction.pressed.take();
                self.ui.drag = None;
                self.ui.interaction.hovered = action;
                if pressed.is_some() && pressed == action {
                    self.perform_action(action.unwrap(), window);
                }
                window.invalidate();
            }
            MouseEventKind::VertWheel(_) | MouseEventKind::HorzWheel(_)
                if matches!(event.kind, MouseEventKind::VertWheel(_))
                    || event.precise_scroll_delta.is_some() =>
            {
                if self.scroll_event(&event, window) {
                    window.invalidate();
                }
            }
            _ if event.mouse_buttons == MouseButtons::NONE => {
                if self.ui.interaction.pressed.take().is_some() {
                    window.invalidate();
                }
            }
            _ => {}
        }
    }

    fn action_at(&self, x: f32, y: f32) -> Option<crate::ui::HitTarget<SettingsAction>> {
        self.ui_context.hit_test(x, y)
    }

    fn scroll_event(&mut self, event: &MouseEvent, window: &Window) -> bool {
        let sidebar_width = self.ui.sidebar.width;
        let sidebar_area = rect(0.0, HEADER_HEIGHT, sidebar_width, self.content_bottom());
        let content_area = rect(
            sidebar_width + 1.0,
            0.0,
            self.dimensions.pixel_width as f32 - sidebar_width - 1.0,
            self.content_bottom(),
        );
        if crate::ui::apply_wheel_to_area(event, sidebar_area, &mut self.ui.sidebar_scroll) {
            self.show_sidebar_scrollbar(window);
            return true;
        }
        if crate::ui::apply_wheel_to_area(event, content_area, &mut self.ui.content_scroll) {
            self.show_content_scrollbar(window);
            return true;
        }
        false
    }

    fn show_sidebar_scrollbar(&mut self, window: &Window) {
        self.ui.sidebar_scrollbar_visible_until =
            Some(Instant::now() + std::time::Duration::from_millis(900));
        Self::schedule_scrollbar_hide(window);
    }

    fn show_content_scrollbar(&mut self, window: &Window) {
        self.ui.content_scrollbar_visible_until =
            Some(Instant::now() + std::time::Duration::from_millis(900));
        Self::schedule_scrollbar_hide(window);
    }

    fn schedule_scrollbar_hide(window: &Window) {
        let window = window.clone();
        promise::spawn::spawn_into_main_thread(async move {
            smol::Timer::after(std::time::Duration::from_millis(930)).await;
            window.invalidate();
        })
        .detach();
    }

    fn schedule_memory_monitor_tick(&self, window: &Window, generation: u64) {
        let window = window.clone();
        promise::spawn::spawn_into_main_thread(async move {
            smol::Timer::after(Duration::from_millis(1500)).await;
            SETTINGS_WINDOW.with(|slot| {
                let Some(settings) = slot.borrow().as_ref().cloned() else {
                    return;
                };
                let mut settings = settings.borrow_mut();
                if !settings.ui.memory_monitoring
                    || settings.ui.memory_monitor_generation != generation
                {
                    return;
                }
                let snapshot = capture_memory_snapshot(false);
                log::info!("settings memory diagnostics: {}", snapshot.log_line());
                settings.ui.memory_snapshot = Some(snapshot);
                window.invalidate();
                settings.schedule_memory_monitor_tick(&window, generation);
            });
        })
        .detach();
    }

    fn settings_resource_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "Settings window: backend={} size={}x{} dpi={}",
            if self.webgpu.is_some() {
                "WebGpu"
            } else {
                "none"
            },
            self.dimensions.pixel_width,
            self.dimensions.pixel_height,
            self.dimensions.dpi
        )];

        if let Some(render_state) = self.render_state.as_ref() {
            let stats = render_state.stats();
            lines.push(format!(
                "Settings window: render_backend={} atlas={} glyphs={} svg_icons={} rotated_icons={} images={} frames={} blocks={} colors={} cursor_glyphs={}",
                stats.backend,
                stats.atlas_size,
                stats.glyphs,
                stats.svg_icons,
                stats.rotated_svg_icons,
                stats.decoded_images,
                stats.image_frames,
                stats.block_glyphs,
                stats.color_sprites,
                stats.cursor_glyphs,
            ));
            lines.push(format!(
                "Settings window: layers={} vertex_buffers={} quad_capacity={} line_glyphs={}",
                stats.layers, stats.vertex_buffers, stats.layer_quads, stats.line_glyphs
            ));
        } else {
            lines.push("Settings window: render_state=none".to_string());
        }

        lines
    }

    fn memory_resource_lines(&self) -> Vec<String> {
        let mut lines = self.settings_resource_lines();
        if self.ui.main_window_resource_lines.is_empty() {
            lines.push("Main windows: not captured yet; use Refresh Now".to_string());
        } else {
            lines.extend(self.ui.main_window_resource_lines.clone());
        }
        lines
    }

    fn request_main_window_resource_stats(&mut self) {
        let Some(front_end) = crate::frontend::try_front_end() else {
            self.ui.main_window_resource_lines =
                vec!["Main windows: frontend unavailable".to_string()];
            return;
        };
        let windows = front_end.gui_windows();
        if windows.is_empty() {
            self.ui.main_window_resource_lines = vec!["Main windows: none".to_string()];
            return;
        }

        self.ui.main_window_resource_lines =
            vec![format!("Main windows: {} pending", windows.len())];
        for (idx, gui_window) in windows.into_iter().enumerate() {
            let label = format!("Main window {}", idx + 1);
            gui_window
                .window
                .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                    move |term_window| {
                        let lines = term_window.memory_resource_lines(&label);
                        SETTINGS_WINDOW.with(|slot| {
                            let Some(settings) = slot.borrow().as_ref().cloned() else {
                                return;
                            };
                            let mut settings = settings.borrow_mut();
                            settings
                                .ui
                                .main_window_resource_lines
                                .retain(|line| !line.contains(" pending"));
                            settings.ui.main_window_resource_lines.extend(lines);
                            if let Some(window) = settings.window.as_ref() {
                                window.invalidate();
                            }
                        });
                    },
                )));
        }
    }

    fn schedule_copied_state_clear(&self, window: &Window) {
        let window = window.clone();
        promise::spawn::spawn_into_main_thread(async move {
            smol::Timer::after(Duration::from_millis(1450)).await;
            SETTINGS_WINDOW.with(|slot| {
                let Some(settings) = slot.borrow().as_ref().cloned() else {
                    return;
                };
                let mut settings = settings.borrow_mut();
                if settings
                    .ui
                    .memory_snapshot_copied_until
                    .is_some_and(|until| Instant::now() >= until)
                {
                    settings.ui.memory_snapshot_copied_until = None;
                    window.invalidate();
                }
            });
        })
        .detach();
    }

    fn key_event(&mut self, event: KeyEvent, window: &Window) -> bool {
        if !event.key_is_down {
            return false;
        }

        let Some(focused) = self.ui.interaction.focused else {
            return false;
        };

        if event
            .modifiers
            .intersects(Modifiers::SUPER | Modifiers::CTRL)
            && !event.modifiers.intersects(Modifiers::ALT)
        {
            return match event.key {
                KeyCode::Char('a') | KeyCode::Char('A') => self.select_all_focused_input(focused),
                KeyCode::Char('c') | KeyCode::Char('C') => self.copy_focused_input(focused, window),
                KeyCode::Char('v') | KeyCode::Char('V') => {
                    self.paste_focused_input_from_clipboard(focused, window)
                }
                _ => false,
            };
        }

        if event
            .modifiers
            .intersects(Modifiers::SUPER | Modifiers::CTRL | Modifiers::ALT)
        {
            return false;
        }

        match event.key {
            KeyCode::Char('\u{8}') | KeyCode::Char('\u{7f}') => {
                self.backspace_focused_input(focused)
            }
            KeyCode::Char('\u{1b}') | KeyCode::Char('\r') => {
                self.ui.interaction.focused = None;
                true
            }
            KeyCode::Char(ch) => {
                if !ch.is_control() {
                    self.push_focused_input(focused, &ch.to_string())
                } else {
                    false
                }
            }
            KeyCode::Composed(text) => self.push_focused_input(focused, &text),
            _ => false,
        }
    }

    fn focused_input_text(&self, focused: SettingsAction) -> Option<&str> {
        match focused {
            SettingsAction::SearchInput => Some(&self.ui.search.text),
            SettingsAction::FontFamilyInput => Some(&self.ui.font_family_input.text),
            _ => None,
        }
    }

    fn select_all_focused_input(&mut self, focused: SettingsAction) -> bool {
        match focused {
            SettingsAction::SearchInput => {
                self.ui.search.select_all();
                true
            }
            SettingsAction::FontFamilyInput => {
                self.ui.font_family_input.select_all();
                true
            }
            _ => false,
        }
    }

    fn copy_focused_input(&mut self, focused: SettingsAction, window: &Window) -> bool {
        let Some(text) = self.focused_input_text(focused) else {
            return false;
        };
        window.set_clipboard(Clipboard::Clipboard, text.to_string());
        true
    }

    fn paste_focused_input_from_clipboard(
        &mut self,
        focused: SettingsAction,
        window: &Window,
    ) -> bool {
        let future = window.get_clipboard(Clipboard::Clipboard);
        let window = window.clone();
        promise::spawn::spawn(async move {
            if let Ok(text) = future.await {
                promise::spawn::spawn_into_main_thread(async move {
                    SETTINGS_WINDOW.with(|slot| {
                        let Some(settings) = slot.borrow().as_ref().cloned() else {
                            return;
                        };
                        let mut settings = settings.borrow_mut();
                        if settings.ui.interaction.focused == Some(focused)
                            && settings.push_focused_input(focused, &text)
                        {
                            window.invalidate();
                        }
                    });
                })
                .detach();
            }
        })
        .detach();
        true
    }

    fn backspace_focused_input(&mut self, focused: SettingsAction) -> bool {
        match focused {
            SettingsAction::SearchInput => {
                self.ui.search.backspace();
                self.ui.sidebar_scroll.reset();
                self.sync_selected_section_with_search();
                true
            }
            SettingsAction::FontFamilyInput => {
                self.ui.font_family_input.backspace();
                self.sync_native_terminal_inputs();
                true
            }
            _ => false,
        }
    }

    fn push_focused_input(&mut self, focused: SettingsAction, text: &str) -> bool {
        match focused {
            SettingsAction::SearchInput => {
                self.ui.search.push_text(text);
                self.ui.sidebar_scroll.reset();
                self.sync_selected_section_with_search();
                true
            }
            SettingsAction::FontFamilyInput => {
                self.ui.font_family_input.push_text(text);
                self.sync_native_terminal_inputs();
                true
            }
            _ => false,
        }
    }

    fn sync_native_terminal_inputs(&mut self) {
        let family = self.ui.font_family_input.text.trim();
        self.native_settings.terminal.font_family = if family.is_empty() {
            None
        } else {
            Some(family.to_string())
        };
        self.save_and_apply_native_terminal_settings();
    }

    fn sync_selected_section_with_search(&mut self) {
        let sections = self.filtered_sections();
        if !sections.is_empty() && !sections.contains(&self.selected) {
            self.selected = sections[0];
            self.ui.content_scroll.reset();
        }
    }

    fn current_terminal_font_size_value(&self) -> f64 {
        self.ui
            .font_size_input
            .text
            .trim()
            .parse::<f64>()
            .ok()
            .or(self.native_settings.terminal.font_size)
            .unwrap_or_else(|| configuration().font_size)
    }

    fn step_terminal_font_size(&mut self, delta: f64) {
        let value = (self.current_terminal_font_size_value() + delta).clamp(8.0, 48.0);
        self.ui.font_size_input.text = format!("{value:.1}");
        self.native_settings.terminal.font_size = Some(value);
        self.save_and_apply_native_terminal_settings();
    }

    fn reset_terminal_font_size(&mut self) {
        let config = configuration();
        self.ui.font_size_input.text = format!("{:.1}", config.font_size);
        self.native_settings.terminal.font_size = None;
        self.save_and_apply_native_terminal_settings();
    }

    fn current_chrome_font_size_value(&self, area: ChromeFontArea) -> f64 {
        let value = match area {
            ChromeFontArea::Settings => self.native_settings.chrome.settings_font_size,
            ChromeFontArea::Sidebar => self.native_settings.chrome.sidebar_font_size,
            ChromeFontArea::TabBar => self.native_settings.chrome.tab_font_size,
            ChromeFontArea::PaneHeader => self.native_settings.chrome.pane_header_font_size,
        };
        value.unwrap_or_else(|| area.default_size())
    }

    fn set_chrome_font_size_value(&mut self, area: ChromeFontArea, value: Option<f64>) {
        match area {
            ChromeFontArea::Settings => self.native_settings.chrome.settings_font_size = value,
            ChromeFontArea::Sidebar => self.native_settings.chrome.sidebar_font_size = value,
            ChromeFontArea::TabBar => self.native_settings.chrome.tab_font_size = value,
            ChromeFontArea::PaneHeader => self.native_settings.chrome.pane_header_font_size = value,
        }
    }

    fn step_chrome_font_size(&mut self, area: ChromeFontArea, delta: f64) {
        let value = (self.current_chrome_font_size_value(area) + delta).clamp(10.0, 28.0);
        self.set_chrome_font_size_value(area, Some(value));
        self.save_native_chrome_settings(area);
    }

    fn reset_chrome_font_size(&mut self, area: ChromeFontArea) {
        self.set_chrome_font_size_value(area, None);
        self.save_native_chrome_settings(area);
    }

    fn current_settings_font_weight_value(&self) -> f64 {
        crate::native_settings::settings_font_weight(&self.native_settings) as f64
    }

    fn step_settings_font_weight(&mut self, delta: i16) {
        let current = crate::native_settings::settings_font_weight(&self.native_settings) as i16;
        let value = (current + delta).clamp(300, 800) as u16;
        self.native_settings.chrome.settings_font_weight = Some(value);
        self.save_native_chrome_settings(ChromeFontArea::Settings);
    }

    fn reset_settings_font_weight(&mut self) {
        self.native_settings.chrome.settings_font_weight = None;
        self.save_native_chrome_settings(ChromeFontArea::Settings);
    }

    fn save_native_chrome_settings(&mut self, area: ChromeFontArea) {
        match crate::native_settings::save(&self.native_settings) {
            Ok(()) => {
                if area == ChromeFontArea::Settings {
                    if let Err(err) = self.reload_settings_fonts() {
                        self.status = format!("Unable to load Settings font: {err:#}");
                        return;
                    }
                }
                if let Some(front_end) = crate::frontend::try_front_end() {
                    front_end.invalidate_all_windows();
                }
                self.status = format!("{} saved.", area.label());
            }
            Err(err) => {
                self.status = format!("Unable to save {}: {err:#}", area.label());
            }
        }
    }

    fn reload_settings_fonts(&mut self) -> anyhow::Result<()> {
        let settings_font_size = crate::native_settings::settings_font_size(&self.native_settings);
        let settings_font_weight =
            crate::native_settings::settings_font_weight(&self.native_settings);
        self.ui_font = self
            .fonts
            .command_palette_font_with_size_and_weight(settings_font_size, settings_font_weight)?;
        self.title_font = self
            .fonts
            .title_font_with_size_and_weight(settings_font_size + 4.0, settings_font_weight)?;
        self.metrics = RenderMetrics::with_font_metrics(&self.ui_font.metrics());
        Ok(())
    }

    fn save_and_apply_native_terminal_settings(&mut self) {
        match crate::native_settings::save(&self.native_settings) {
            Ok(()) => {
                self.apply_terminal_font_size_to_open_windows();
                self.status =
                    "Terminal settings saved; font size applied to open windows.".to_string();
            }
            Err(err) => {
                self.status = format!("Unable to save terminal settings: {err:#}");
            }
        }
    }

    fn apply_terminal_font_size_to_open_windows(&self) {
        let font_size = self
            .native_settings
            .terminal
            .font_size
            .unwrap_or_else(|| configuration().font_size);
        let Some(front_end) = crate::frontend::try_front_end() else {
            return;
        };
        for gui_window in front_end.gui_windows() {
            gui_window
                .window
                .notify(crate::termwindow::TermWindowNotif::Apply(Box::new(
                    move |term_window| {
                        if !font_size.is_finite()
                            || font_size <= 0.0
                            || term_window.config.font_size <= 0.0
                        {
                            return;
                        }
                        let font_scale =
                            (font_size / term_window.config.font_size).clamp(0.25, 4.0);
                        if let Some(window) = term_window.window.as_ref().cloned() {
                            term_window.adjust_font_scale(font_scale, &window);
                        }
                    },
                )));
        }
    }

    fn effective_appearance(&self) -> Appearance {
        self.native_settings
            .appearance
            .theme_mode
            .effective_appearance(self.appearance)
    }

    fn palette(&self) -> SettingsPalette {
        let appearance = self.effective_appearance();
        let ui = UiPalette::for_appearance(appearance);
        match appearance {
            Appearance::Light | Appearance::LightHighContrast => SettingsPalette {
                window_bg: ui.window_bg,
                sidebar_bg: ui.workspace_sidebar_bg,
                separator: ui.separator,
                search_bg: ui.control_bg,
                search_border: ui.control_border,
                nav_hover_bg: ui.control_hover_bg,
                nav_pressed_bg: ui.control_pressed_bg,
                nav_selected_bg: ui.control_bg,
                control_bg: ui.control_bg,
                control_hover_bg: ui.control_hover_bg,
                control_pressed_bg: ui.control_pressed_bg,
                control_border: ui.control_border,
                card_bg: rgba(255, 255, 255, 0.72),
                title: ui.text,
                text: ui.text,
                secondary_text: ui.secondary_text,
                muted_text: ui.muted_text,
                selected_text: ui.text,
                rule: ui.separator,
            },
            Appearance::Dark | Appearance::DarkHighContrast => SettingsPalette {
                window_bg: ui.window_bg,
                sidebar_bg: ui.workspace_sidebar_bg,
                separator: ui.separator,
                search_bg: ui.control_bg,
                search_border: ui.control_border,
                nav_hover_bg: ui.control_hover_bg,
                nav_pressed_bg: ui.control_pressed_bg,
                nav_selected_bg: ui.control_bg,
                control_bg: ui.control_bg,
                control_hover_bg: ui.control_hover_bg,
                control_pressed_bg: ui.control_pressed_bg,
                control_border: ui.control_border,
                card_bg: rgba(30, 30, 32, 0.78),
                title: ui.text,
                text: ui.text,
                secondary_text: ui.secondary_text,
                muted_text: ui.muted_text,
                selected_text: ui.text,
                rule: ui.separator,
            },
        }
    }

    fn settings_row_step(&self) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        (cell_height * 2.15 + 42.0).max(116.0).ceil()
    }

    fn settings_card_top_padding(&self) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        (cell_height * 0.62).clamp(22.0, 30.0).ceil()
    }

    fn settings_card_bottom_padding(&self) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        (cell_height * 0.72).clamp(26.0, 36.0).ceil()
    }

    fn settings_row_visual_height(&self) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        (cell_height + 46.0).max(CONTROL_HEIGHT + 10.0).ceil()
    }

    fn settings_row_description_y(&self, y: f32) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        y + (cell_height + 8.0).max(34.0)
    }

    fn settings_section_card_gap(&self) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        (cell_height * 1.45).clamp(52.0, 72.0).ceil()
    }

    fn settings_card_height(&self, row_count: usize) -> f32 {
        if row_count == 0 {
            return 0.0;
        }
        self.settings_card_top_padding()
            + self.settings_row_step() * row_count.saturating_sub(1) as f32
            + self.settings_row_visual_height()
            + self.settings_card_bottom_padding()
    }

    fn settings_card_geometry(&self, section_y: f32, _row_count: usize) -> (f32, f32) {
        let card_y = section_y + self.settings_section_card_gap();
        let first_row_y = card_y + self.settings_card_top_padding();
        (card_y, first_row_y)
    }

    fn settings_content_extent(&self, bottom_y: f32) -> f32 {
        (bottom_y + 65.0).max(self.content_bottom())
    }

    fn developer_mode_enabled(&self) -> bool {
        self.native_settings.developer.developer_mode
    }

    fn visible_sections(&self) -> Vec<SettingsSection> {
        let mut sections = Vec::with_capacity(BASE_SECTIONS.len() + DEVELOPER_SECTIONS.len());
        for section in BASE_SECTIONS {
            if *section == SettingsSection::About && self.developer_mode_enabled() {
                sections.extend_from_slice(DEVELOPER_SECTIONS);
            }
            sections.push(*section);
        }
        sections
    }

    fn section_is_visible(&self, section: SettingsSection) -> bool {
        self.visible_sections().contains(&section)
    }

    fn clamp_sidebar_to_window(&mut self) {
        let dynamic_max = (self.dimensions.pixel_width as f32 * 0.38)
            .max(self.ui.sidebar.min_width)
            .min(self.ui.sidebar.max_width);
        if self.ui.sidebar.width > dynamic_max {
            self.ui.sidebar.width = dynamic_max;
        }
    }

    fn perform_action(&mut self, action: SettingsAction, window: &Window) {
        match action {
            SettingsAction::Select(section) => {
                self.selected = section;
                self.ui.content_scroll.reset();
                self.ui.open_dropdown = None;
            }
            SettingsAction::OpenConfigFile => {
                self.ui.open_dropdown = None;
                if let Some(path) = config::configuration_file() {
                    self.status = format!("Opening active config {}", path.display());
                    Self::open_path(path);
                } else {
                    self.status = "Using built-in defaults; no config file to open".to_string();
                }
            }
            SettingsAction::ShowCompatibilityStatus => {
                self.ui.open_dropdown = None;
                self.status = match config::configuration_file() {
                    Some(path) => format!(
                        "Compatibility view is using {}. GUI import is intentionally pending.",
                        path.display()
                    ),
                    None => {
                        "No config file is loaded; ThinkTerm is using built-in defaults".to_string()
                    }
                };
            }
            SettingsAction::ToggleMainWindowFrameRestore => {
                self.ui.open_dropdown = None;
                self.native_settings.window.restore_main_window_frame =
                    !self.native_settings.window.restore_main_window_frame;
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        self.status = if self.native_settings.window.restore_main_window_frame {
                            "Main window frame restore enabled for new macOS windows.".to_string()
                        } else {
                            "Main window frame restore disabled for new macOS windows.".to_string()
                        };
                    }
                    Err(err) => {
                        self.status = format!("Unable to save window restore setting: {err:#}");
                    }
                }
            }
            SettingsAction::ToggleDeveloperMode => {
                self.ui.open_dropdown = None;
                self.native_settings.developer.developer_mode =
                    !self.native_settings.developer.developer_mode;
                if !self.developer_mode_enabled() && !self.section_is_visible(self.selected) {
                    self.selected = SettingsSection::Developer;
                    self.ui.content_scroll.reset();
                }
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        self.status = if self.developer_mode_enabled() {
                            "Developer mode enabled. Extra diagnostics tabs are now visible."
                                .to_string()
                        } else {
                            "Developer mode disabled. Diagnostics tabs are hidden.".to_string()
                        };
                    }
                    Err(err) => {
                        self.status = format!("Unable to save developer mode: {err:#}");
                    }
                }
            }
            SettingsAction::ToggleMemoryMonitoring => {
                self.ui.open_dropdown = None;
                self.ui.memory_monitoring = !self.ui.memory_monitoring;
                self.ui.memory_monitor_generation =
                    self.ui.memory_monitor_generation.wrapping_add(1);
                if self.ui.memory_monitoring {
                    let snapshot = capture_memory_snapshot(false);
                    log::info!("settings memory diagnostics: {}", snapshot.log_line());
                    self.ui.memory_snapshot = Some(snapshot);
                    self.request_main_window_resource_stats();
                    self.status = "Memory diagnostics are running manually.".to_string();
                    self.schedule_memory_monitor_tick(window, self.ui.memory_monitor_generation);
                } else {
                    self.status = "Memory diagnostics stopped.".to_string();
                }
            }
            SettingsAction::RefreshMemorySnapshot => {
                self.ui.open_dropdown = None;
                let snapshot = capture_memory_snapshot(true);
                log::info!("settings memory diagnostics: {}", snapshot.log_line());
                self.ui.memory_snapshot = Some(snapshot);
                self.request_main_window_resource_stats();
                self.status = "Memory snapshot refreshed.".to_string();
            }
            SettingsAction::CopyMemorySnapshot => {
                self.ui.open_dropdown = None;
                if self
                    .ui
                    .memory_snapshot
                    .as_ref()
                    .is_none_or(|snapshot| !snapshot.has_vmmap_breakdown())
                {
                    self.ui.memory_snapshot = Some(capture_memory_snapshot(true));
                }
                if let Some(snapshot) = &self.ui.memory_snapshot {
                    let mut summary = snapshot.summary_for_clipboard();
                    summary.push_str("\n\nThinkTerm Resource Stats\n");
                    summary.push_str(&self.memory_resource_lines().join("\n"));
                    window.set_clipboard(Clipboard::Clipboard, summary);
                    self.request_main_window_resource_stats();
                    self.ui.memory_snapshot_copied_until =
                        Some(Instant::now() + Duration::from_millis(1400));
                    self.status = "Memory snapshot copied.".to_string();
                    self.schedule_copied_state_clear(window);
                }
            }
            SettingsAction::ToggleThemeModeMenu => {
                self.ui.open_dropdown =
                    if self.ui.open_dropdown == Some(SettingsDropdown::ThemeMode) {
                        None
                    } else {
                        Some(SettingsDropdown::ThemeMode)
                    };
            }
            SettingsAction::SetThemeMode(mode) => {
                self.native_settings.appearance.theme_mode = mode;
                self.ui.open_dropdown = None;
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        crate::native_settings::apply_to_app(&self.native_settings);
                        if let Some(front_end) = crate::frontend::try_front_end() {
                            front_end.invalidate_all_windows();
                        }
                        self.status = format!("Theme mode is now {}.", mode.label());
                    }
                    Err(err) => {
                        self.status = format!("Unable to save theme mode: {err:#}");
                    }
                }
            }
            SettingsAction::ToggleAppIconMenu => {
                self.ui.open_dropdown = if self.ui.open_dropdown == Some(SettingsDropdown::AppIcon)
                {
                    None
                } else {
                    Some(SettingsDropdown::AppIcon)
                };
            }
            SettingsAction::SetAppIcon(icon) => {
                self.native_settings.appearance.app_icon = icon;
                self.ui.open_dropdown = None;
                match crate::native_settings::save(&self.native_settings) {
                    Ok(()) => {
                        crate::native_settings::apply_to_app(&self.native_settings);
                        self.status = format!("App icon is now {}.", icon.label());
                    }
                    Err(err) => {
                        self.status = format!("Unable to save app icon: {err:#}");
                    }
                }
            }
            SettingsAction::SearchInput => {
                self.ui.interaction.focused = Some(SettingsAction::SearchInput);
            }
            SettingsAction::DecreaseFontSize => self.step_terminal_font_size(-1.0),
            SettingsAction::IncreaseFontSize => self.step_terminal_font_size(1.0),
            SettingsAction::ResetFontSize => self.reset_terminal_font_size(),
            SettingsAction::DecreaseChromeFontSize(area) => self.step_chrome_font_size(area, -1.0),
            SettingsAction::IncreaseChromeFontSize(area) => self.step_chrome_font_size(area, 1.0),
            SettingsAction::ResetChromeFontSize(area) => self.reset_chrome_font_size(area),
            SettingsAction::DecreaseSettingsFontWeight => self.step_settings_font_weight(-100),
            SettingsAction::IncreaseSettingsFontWeight => self.step_settings_font_weight(100),
            SettingsAction::ResetSettingsFontWeight => self.reset_settings_font_weight(),
            SettingsAction::FontFamilyInput => {
                self.ui.interaction.focused = Some(SettingsAction::FontFamilyInput);
            }
            SettingsAction::ClearSearch => {
                self.ui.search.clear();
                self.ui.sidebar_scroll.reset();
                self.sync_selected_section_with_search();
                self.ui.interaction.focused = Some(SettingsAction::SearchInput);
            }
            SettingsAction::SidebarResize
            | SettingsAction::SidebarScrollArea
            | SettingsAction::ContentScrollArea => {}
        }
    }

    fn do_paint(&mut self, window: &Window) -> bool {
        let paint_start = crate::perf::now();
        let animating = self.advance_scroll_animations(Instant::now());
        match self.do_paint_webgpu() {
            Ok(ok) => {
                crate::perf::log_duration("settings_paint", paint_start);
                if animating {
                    window.invalidate();
                }
                ok
            }
            Err(err) => {
                crate::perf::log_duration("settings_paint_failed", paint_start);
                log::error!("settings window webgpu paint failed: {err:#}");
                false
            }
        }
    }

    fn advance_scroll_animations(&mut self, now: Instant) -> bool {
        self.ui.sidebar_scroll.advance_animation(now)
            | self.ui.content_scroll.advance_animation(now)
    }

    fn do_paint_webgpu(&mut self) -> anyhow::Result<bool> {
        let webgpu = Rc::clone(
            self.webgpu
                .as_ref()
                .context("settings webgpu state not initialized")?,
        );
        match self.do_paint_webgpu_impl(&webgpu) {
            Ok(ok) => Ok(ok),
            Err(err) => {
                match err.downcast_ref::<wgpu::SurfaceError>() {
                    Some(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                        webgpu.resize(self.dimensions);
                        return self.do_paint_webgpu_impl(&webgpu);
                    }
                    _ => {}
                }
                Err(err)
            }
        }
    }

    fn do_paint_webgpu_impl(&mut self, webgpu: &WebGpuState) -> anyhow::Result<bool> {
        for _ in 0..3 {
            let paint_pass_start = crate::perf::now();
            match self.paint_pass() {
                Ok(()) => {
                    crate::perf::log_duration("settings_paint_pass", paint_pass_start);
                    match self.render_state.as_mut().unwrap().allocated_more_quads() {
                        Ok(true) => continue,
                        Ok(false) => break,
                        Err(err) => {
                            log::error!("settings window quad allocation failed: {err:#}");
                            break;
                        }
                    }
                }
                Err(err) => {
                    if let Some(&OutOfTextureSpace {
                        size: Some(size),
                        current_size,
                    }) = err.root_cause().downcast_ref::<OutOfTextureSpace>()
                    {
                        let size = size.max(current_size);
                        crate::perf::log_counter("settings_atlas_reallocate", size);
                        if let Err(err) = self
                            .render_state
                            .as_mut()
                            .unwrap()
                            .recreate_texture_atlas(&self.fonts, &self.metrics, Some(size))
                        {
                            log::error!("settings window texture atlas resize failed: {err:#}");
                            break;
                        }
                        continue;
                    }
                    log::error!("settings window paint failed: {err:#}");
                    break;
                }
            }
        }

        let clear_color = wgpu_color(self.palette().window_bg);
        let render_state = self
            .render_state
            .as_ref()
            .context("settings render state not initialized")?;
        draw_webgpu_layers(
            webgpu,
            render_state,
            self.dimensions,
            [1.0, 1.0, 1.0],
            0,
            clear_color,
        )?;
        Ok(true)
    }

    fn paint_pass(&mut self) -> anyhow::Result<()> {
        if let Some(render_state) = self.render_state.as_ref() {
            for layer in render_state.layers.borrow().iter() {
                layer.clear_quad_allocation();
            }
        }

        self.ui_context.clear();
        let layer = self
            .render_state
            .as_ref()
            .context("settings render state not initialized")?
            .layer_for_zindex(0)?;
        let mut layers = layer.quad_allocator();

        self.paint_background(&mut layers)?;
        self.paint_sidebar(&mut layers)?;
        self.paint_content(&mut layers)?;

        Ok(())
    }

    fn paint_background(&self, layers: &mut TripleLayerQuadAllocator<'_>) -> anyhow::Result<()> {
        let palette = self.palette();
        let width = self.dimensions.pixel_width as f32;
        let height = self.dimensions.pixel_height as f32;
        let sidebar_width = self.ui.sidebar.width;
        self.draw_rect(layers, 0, 0.0, 0.0, width, height, palette.window_bg)?;
        self.draw_rect(
            layers,
            0,
            0.0,
            0.0,
            sidebar_width,
            height,
            palette.sidebar_bg,
        )?;
        self.draw_rect(
            layers,
            0,
            sidebar_width,
            0.0,
            1.0,
            height,
            palette.separator,
        )?;
        Ok(())
    }

    fn paint_sidebar(&mut self, layers: &mut TripleLayerQuadAllocator<'_>) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let sidebar_title_font = Rc::clone(&self.sidebar_title_font);
        let nav_font = Rc::clone(&self.ui_font);
        let tokens = self.ui.tokens;
        let sidebar_width = self.ui.sidebar.width;
        let sidebar_icon_size = ((self.metrics.cell_size.height as f32 + 8.0)
            .clamp(24.0, 34.0)
            .round()) as usize;

        self.draw_text(
            layers,
            &sidebar_title_font,
            tokens.sidebar_padding + 6.0,
            SIDEBAR_TITLE_Y,
            "ThinkTerm",
            palette.title,
            sidebar_width - tokens.sidebar_padding * 2.0,
        )?;

        let search_rect = rect(
            tokens.sidebar_padding,
            SIDEBAR_SEARCH_Y,
            sidebar_width - tokens.sidebar_padding * 2.0,
            tokens.control_height,
        );
        let search_text = self.ui.search.text.clone();
        self.paint_text_input(
            layers,
            TextInputSpec {
                placeholder: "Search settings...",
                text: &search_text,
                rect: search_rect,
                focused: self.ui.interaction.focused == Some(SettingsAction::SearchInput),
                selected_all: self.ui.search.selected_all,
                action: SettingsAction::SearchInput,
            },
        )?;
        self.draw_svg_icon(
            layers,
            SettingsIcon::Search.svg(),
            search_rect.origin.x + 15.0,
            search_rect.origin.y + (search_rect.size.height - sidebar_icon_size as f32) / 2.0,
            sidebar_icon_size as f32,
            palette.muted_text,
        )?;
        if !self.ui.search.is_empty() {
            let clear_size = 34.0;
            let clear_icon_size = 24.0;
            let clear_rect = rect(
                search_rect.origin.x + search_rect.size.width - clear_size - 10.0,
                search_rect.origin.y + (search_rect.size.height - clear_size) / 2.0,
                clear_size,
                clear_size,
            );
            self.ui_context
                .push(clear_rect, WidgetKind::Button, SettingsAction::ClearSearch);
            if self.ui.interaction.hovered == Some(SettingsAction::ClearSearch)
                || self.ui.interaction.pressed == Some(SettingsAction::ClearSearch)
            {
                self.draw_rounded_rect(
                    layers,
                    0,
                    clear_rect.origin.x,
                    clear_rect.origin.y,
                    clear_rect.size.width,
                    clear_rect.size.height,
                    palette.control_hover_bg,
                    14.0,
                )?;
            }
            self.draw_svg_icon(
                layers,
                SettingsIcon::Clear.svg(),
                clear_rect.origin.x + (clear_rect.size.width - clear_icon_size) / 2.0,
                clear_rect.origin.y + (clear_rect.size.height - clear_icon_size) / 2.0,
                clear_icon_size,
                palette.secondary_text,
            )?;
        }

        let handle_rect = rect(
            sidebar_width - tokens.resize_handle_width / 2.0,
            0.0,
            tokens.resize_handle_width,
            self.dimensions.pixel_height as f32,
        );
        self.ui_context.push(
            handle_rect,
            WidgetKind::ResizeHandle,
            SettingsAction::SidebarResize,
        );
        let handle_color = if self.ui.interaction.hovered == Some(SettingsAction::SidebarResize)
            || matches!(self.ui.drag, Some(SettingsDrag::SidebarResize { .. }))
        {
            palette.nav_selected_bg
        } else {
            palette.separator
        };
        self.draw_rect(
            layers,
            0,
            sidebar_width - 1.0,
            0.0,
            2.0,
            self.dimensions.pixel_height as f32,
            handle_color,
        )?;

        let sections = self.filtered_sections();
        let list_top = SIDEBAR_LIST_TOP;
        let list_bottom = (self.dimensions.pixel_height as f32 - 16.0).max(list_top);
        let list_height = list_bottom - list_top;
        let content_extent = sections.len() as f32 * NAV_ROW_STEP + 8.0;
        self.ui
            .sidebar_scroll
            .set_extents(list_height, content_extent.max(list_height));
        let list_area = rect(0.0, list_top, sidebar_width, list_height);
        self.ui_context.push(
            list_area,
            WidgetKind::ScrollArea,
            SettingsAction::SidebarScrollArea,
        );

        let mut y = list_top - self.ui.sidebar_scroll.offset;
        if sections.is_empty() {
            self.draw_text(
                layers,
                &ui_font,
                tokens.sidebar_padding + 12.0,
                list_top + 18.0,
                "No settings found",
                palette.muted_text,
                sidebar_width - tokens.sidebar_padding * 2.0 - 24.0,
            )?;
        }

        for section in sections {
            if y + NAV_ROW_HEIGHT < list_top || y > list_bottom {
                y += NAV_ROW_STEP;
                continue;
            }
            let action = SettingsAction::Select(section);
            let selected = section == self.selected;
            let hovered = self.ui.interaction.hovered == Some(action);
            let pressed = self.ui.interaction.pressed == Some(action);
            let row_y = y - 6.0;
            let row_x = tokens.sidebar_padding;
            let row_width = sidebar_width - tokens.sidebar_padding * 2.0;
            let row_bg = if selected {
                Some(palette.nav_selected_bg)
            } else if pressed {
                Some(palette.nav_pressed_bg)
            } else if hovered {
                Some(palette.nav_hover_bg)
            } else {
                None
            };
            if let Some(row_bg) = row_bg {
                self.draw_rounded_rect(
                    layers,
                    0,
                    row_x,
                    row_y,
                    row_width,
                    NAV_ROW_HEIGHT,
                    row_bg,
                    NAV_ROW_RADIUS,
                )?;
            }

            self.ui_context.push(
                rect(row_x, row_y, row_width, NAV_ROW_HEIGHT),
                WidgetKind::SidebarRow,
                action,
            );
            let text_color = if selected {
                palette.selected_text
            } else {
                palette.secondary_text
            };
            self.draw_svg_icon(
                layers,
                section.icon().svg(),
                row_x + 14.0,
                row_y + (NAV_ROW_HEIGHT - sidebar_icon_size as f32) / 2.0,
                sidebar_icon_size as f32,
                text_color,
            )?;
            self.draw_text(
                layers,
                &nav_font,
                row_x + 56.0,
                self.control_text_y(row_y, NAV_ROW_HEIGHT),
                section.label(),
                text_color,
                row_width - 76.0,
            )?;
            y += NAV_ROW_STEP;
        }

        self.paint_scrollbar(
            layers,
            list_area,
            self.ui.sidebar_scroll,
            self.sidebar_scrollbar_visible(),
        )?;

        Ok(())
    }

    fn paint_content(&mut self, layers: &mut TripleLayerQuadAllocator<'_>) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let title_font = Rc::clone(&self.title_font);
        let sidebar_width = self.ui.sidebar.width;
        let window_width = self.dimensions.pixel_width as f32;
        let content_gap = if window_width < 980.0 { 30.0 } else { 46.0 };
        let right_margin = if window_width < 980.0 { 34.0 } else { 50.0 };
        let x = sidebar_width + content_gap;
        let max_width = (window_width - x - right_margin).max(280.0);
        let content_area = rect(
            sidebar_width + 1.0,
            0.0,
            self.dimensions.pixel_width as f32 - sidebar_width - 1.0,
            self.content_bottom(),
        );
        self.ui_context.push(
            content_area,
            WidgetKind::ScrollArea,
            SettingsAction::ContentScrollArea,
        );

        let scroll = self.ui.content_scroll.offset;
        self.draw_text(
            layers,
            &title_font,
            x,
            CONTENT_TITLE_Y - scroll,
            self.selected.label(),
            palette.title,
            max_width,
        )?;

        match self.selected {
            SettingsSection::Appearance => self.paint_appearance(layers, x, max_width)?,
            SettingsSection::Compatibility => self.paint_compatibility(layers, x, max_width)?,
            SettingsSection::General => self.paint_general(layers, x, max_width)?,
            SettingsSection::Terminal => self.paint_terminal(layers, x, max_width)?,
            SettingsSection::Workspaces => self.paint_placeholder(
                layers,
                &ui_font,
                x,
                "Workspace list, default workspace, sidebar behavior, and saved layouts.",
                max_width,
            )?,
            SettingsSection::Keymap => self.paint_placeholder(
                layers,
                &ui_font,
                x,
                "Keyboard shortcuts and command palette actions.",
                max_width,
            )?,
            SettingsSection::Developer => self.paint_developer(layers, x, max_width)?,
            SettingsSection::UiKit => self.paint_ui_kit(layers, x, max_width)?,
            SettingsSection::Memory => self.paint_memory_diagnostics(layers, x, max_width)?,
            SettingsSection::About => self.paint_placeholder(
                layers,
                &ui_font,
                x,
                "ThinkTerm version, project information, and app metadata.",
                max_width,
            )?,
        }
        self.paint_scrollbar(
            layers,
            content_area,
            self.ui.content_scroll,
            self.content_scrollbar_visible(),
        )?;
        self.paint_open_dropdown_overlay(layers, x, max_width)?;

        Ok(())
    }

    fn paint_general(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = CONTENT_SECTION_Y - scroll;
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, 4);
        let card_height = self.settings_card_height(4);
        self.ui.content_scroll.set_extents(
            self.content_bottom(),
            self.settings_content_extent(card_y + scroll + card_height),
        );
        let source = Self::config_source_summary();
        let native_path = Self::native_settings_path();
        let native_status = if native_path.exists() {
            native_path.display().to_string()
        } else {
            format!("Defaults; optional file at {}", native_path.display())
        };

        self.draw_text(
            layers,
            &ui_font,
            x,
            section_y,
            "Configuration",
            palette.muted_text,
            max_width,
        )?;

        let card_x = x;
        let card_padding = 36.0;
        let row_x = card_x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        self.paint_group_card(layers, card_x, card_y, max_width, card_height)?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y,
            row_width,
            "Config Source",
            &source,
            if config::configuration_file().is_some() {
                "File"
            } else {
                "Defaults"
            },
            false,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step,
            row_width,
            "ThinkTerm Native Settings",
            &native_status,
            &format!("v{}", self.native_settings.version),
            true,
        )?;
        self.paint_theme_mode_row(
            layers,
            row_x,
            first_row_y + row_step * 2.0,
            row_width,
            "Small ThinkTerm-native state; compatible terminal config stays in wezterm.lua.",
            true,
        )?;
        self.paint_toggle_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 3.0,
            row_width,
            "Restore Main Window Frame",
            "macOS restores the last main terminal window size and position.",
            self.native_settings.window.restore_main_window_frame,
            SettingsAction::ToggleMainWindowFrameRestore,
            true,
        )?;
        Ok(())
    }

    fn paint_appearance(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let config = configuration();
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = CONTENT_SECTION_Y - scroll;
        let theme_row_count = 4;
        let typography_row_count = 5;
        let (theme_card_y, mut y) = self.settings_card_geometry(section_y, theme_row_count);
        let theme_card_height = self.settings_card_height(theme_row_count);
        let typography_title_y =
            theme_card_y + theme_card_height + self.settings_section_card_gap();
        let typography_card_y = typography_title_y + self.settings_section_card_gap().min(54.0);
        let typography_first_row_y = typography_card_y + self.settings_card_top_padding();
        let typography_card_height = self.settings_card_height(typography_row_count);
        self.ui.content_scroll.set_extents(
            self.content_bottom(),
            self.settings_content_extent(typography_card_y + scroll + typography_card_height),
        );

        self.draw_text(
            layers,
            &ui_font,
            x,
            section_y,
            "Theme",
            palette.muted_text,
            max_width,
        )?;

        let card_padding = 36.0;
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        self.paint_group_card(layers, x, theme_card_y, max_width, theme_card_height)?;
        self.paint_theme_mode_row(
            layers,
            row_x,
            y,
            row_width,
            "ThinkTerm-native window appearance preference.",
            false,
        )?;
        y += row_step;
        self.paint_app_icon_row(
            layers,
            row_x,
            y,
            row_width,
            "Switch the running macOS Dock and app switcher icon.",
            true,
        )?;
        y += row_step;
        self.paint_setting_row(
            layers,
            row_x,
            y,
            row_width,
            "Effective Color Scheme",
            "Resolved through the WezTerm-compatible config path.",
            Self::effective_color_scheme_label(&config),
            true,
        )?;
        y += row_step;
        self.paint_setting_row(
            layers,
            row_x,
            y,
            row_width,
            "Config Source",
            &Self::config_source_summary(),
            if config::configuration_file().is_some() {
                "File"
            } else {
                "Defaults"
            },
            true,
        )?;

        self.draw_text(
            layers,
            &ui_font,
            x,
            typography_title_y,
            "Typography",
            palette.muted_text,
            max_width,
        )?;

        self.paint_group_card(
            layers,
            x,
            typography_card_y,
            max_width,
            typography_card_height,
        )?;
        y = typography_first_row_y;
        for area in [
            ChromeFontArea::Settings,
            ChromeFontArea::Sidebar,
            ChromeFontArea::TabBar,
            ChromeFontArea::PaneHeader,
        ] {
            self.paint_font_size_stepper_row(
                layers,
                row_x,
                y,
                row_width,
                area.label(),
                area.description(),
                self.current_chrome_font_size_value(area),
                SettingsAction::ResetChromeFontSize(area),
                SettingsAction::DecreaseChromeFontSize(area),
                SettingsAction::IncreaseChromeFontSize(area),
                area != ChromeFontArea::Settings,
            )?;
            y += row_step;
        }
        self.paint_font_size_stepper_row(
            layers,
            row_x,
            y,
            row_width,
            "Settings UI Font Weight",
            "Controls the Settings window chrome and content text weight.",
            self.current_settings_font_weight_value(),
            SettingsAction::ResetSettingsFontWeight,
            SettingsAction::DecreaseSettingsFontWeight,
            SettingsAction::IncreaseSettingsFontWeight,
            true,
        )?;

        Ok(())
    }

    fn paint_terminal(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let config = configuration();
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = CONTENT_SECTION_Y - scroll;
        let row_count = 4;
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, row_count);
        let card_height = self.settings_card_height(row_count);
        self.ui.content_scroll.set_extents(
            self.content_bottom(),
            self.settings_content_extent(card_y + scroll + card_height),
        );
        let font_size = format!("{:.1} pt", config.font_size);
        let font_family = Self::effective_font_family(&config);

        self.draw_text(
            layers,
            &ui_font,
            x,
            section_y,
            "Effective terminal config",
            palette.muted_text,
            max_width,
        )?;

        let card_padding = 36.0;
        let card_x = x;
        let row_x = card_x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        self.paint_group_card(layers, card_x, card_y, max_width, card_height)?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y,
            row_width,
            "Font Size",
            "Current effective value from the WezTerm-compatible config.",
            &font_size,
            false,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step,
            row_width,
            "Font Family",
            "Current primary terminal font family.",
            &font_family,
            true,
        )?;
        self.paint_font_size_stepper_row(
            layers,
            row_x,
            first_row_y + row_step * 2.0,
            row_width,
            "ThinkTerm Font Size",
            "Saved locally and applied immediately to open terminal windows.",
            self.current_terminal_font_size_value(),
            SettingsAction::ResetFontSize,
            SettingsAction::DecreaseFontSize,
            SettingsAction::IncreaseFontSize,
            true,
        )?;
        let native_font_family = self.ui.font_family_input.text.clone();
        self.paint_text_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 3.0,
            row_width,
            "Native Font Family",
            "Local quick-setting draft. Persistence and terminal application are pending.",
            &native_font_family,
            "JetBrains Mono",
            SettingsAction::FontFamilyInput,
            true,
        )?;
        Ok(())
    }

    fn paint_developer(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = CONTENT_SECTION_Y - scroll;
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, 2);
        let card_height = self.settings_card_height(2);
        let button_y = card_y + card_height + self.settings_section_card_gap();
        self.ui.content_scroll.set_extents(
            self.content_bottom(),
            self.settings_content_extent(button_y + scroll + CONTROL_HEIGHT),
        );

        self.draw_text(
            layers,
            &ui_font,
            x,
            section_y,
            "Keep normal Settings clean. Enable developer mode to reveal internal pages. Memory sampling still has to be started manually.",
            palette.secondary_text,
            max_width,
        )?;

        let card_padding = 36.0;
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y,
            row_width,
            "Developer Mode",
            "Shows internal Settings pages for UI tuning and memory diagnostics.",
            if self.developer_mode_enabled() {
                "On"
            } else {
                "Off"
            },
            false,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step,
            row_width,
            "Visible Developer Tabs",
            "UI Kit and Memory tabs are shown here; diagnostics do not run automatically.",
            if self.developer_mode_enabled() {
                "UI Kit, Memory"
            } else {
                "Hidden"
            },
            true,
        )?;
        self.draw_button(
            layers,
            x,
            button_y,
            self.button_width_for_label(
                if self.developer_mode_enabled() {
                    "Disable Developer Mode"
                } else {
                    "Enable Developer Mode"
                },
                300.0,
            ),
            if self.developer_mode_enabled() {
                "Disable Developer Mode"
            } else {
                "Enable Developer Mode"
            },
            SettingsAction::ToggleDeveloperMode,
        )?;

        Ok(())
    }

    fn paint_ui_kit(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let two_column = max_width >= 940.0;
        let content_extent = if two_column { 1020.0 } else { 1660.0 };
        self.ui
            .content_scroll
            .set_extents(self.content_bottom(), content_extent);
        let scroll = self.ui.content_scroll.offset;

        self.draw_text(
            layers,
            &ui_font,
            x,
            CONTENT_SECTION_Y - scroll,
            "Live settings UI components. The left side is rendered with the same primitives as the real window.",
            palette.secondary_text,
            max_width,
        )?;
        self.draw_rect(
            layers,
            0,
            x,
            CONTENT_RULE_Y - scroll,
            max_width,
            1.0,
            palette.rule,
        )?;

        let preview_width = if two_column {
            (max_width * 0.52).min(620.0)
        } else {
            max_width
        };
        let notes_x = if two_column {
            x + preview_width + 42.0
        } else {
            x
        };
        let notes_width = if two_column {
            (max_width - preview_width - 42.0).max(280.0)
        } else {
            max_width
        };
        let notes_top = if two_column { 244.0 } else { 900.0 };

        self.draw_text(
            layers,
            &ui_font,
            x,
            244.0 - scroll,
            "Component Preview",
            palette.title,
            preview_width,
        )?;
        self.draw_rect(
            layers,
            0,
            x,
            274.0 - scroll,
            preview_width,
            1.0,
            palette.rule,
        )?;
        self.paint_preview_search(layers, x, 298.0 - scroll, preview_width)?;

        self.draw_text(
            layers,
            &ui_font,
            x,
            382.0 - scroll,
            "Sidebar Rows",
            palette.title,
            preview_width,
        )?;
        self.paint_preview_sidebar_row(layers, x, 416.0 - scroll, preview_width, "General", false)?;
        self.paint_preview_sidebar_row(
            layers,
            x,
            470.0 - scroll,
            preview_width,
            "Appearance",
            true,
        )?;

        self.draw_text(
            layers,
            &ui_font,
            x,
            540.0 - scroll,
            "Buttons and Controls",
            palette.title,
            preview_width,
        )?;
        self.paint_preview_button(layers, x, 578.0 - scroll, 250.0, "Normal Button", false)?;
        self.paint_preview_button(
            layers,
            x + 270.0,
            578.0 - scroll,
            220.0,
            "Accent Button",
            true,
        )?;
        self.paint_preview_control(layers, x, 648.0 - scroll, 280.0, "System")?;

        self.draw_text(
            layers,
            &ui_font,
            x,
            730.0 - scroll,
            "Setting Row",
            palette.title,
            preview_width,
        )?;
        self.paint_setting_row(
            layers,
            x,
            800.0 - scroll,
            preview_width,
            "Theme Mode",
            "Follow macOS appearance.",
            "System",
            false,
        )?;

        let component_tokens = [
            StyleToken {
                name: "search.field",
                value: "h56 bg+focus border",
                swatch: Some(palette.search_bg),
            },
            StyleToken {
                name: "nav.active.row",
                value: "h48 system selection",
                swatch: Some(palette.nav_selected_bg),
            },
            StyleToken {
                name: "button.normal",
                value: "h56 rounded hover",
                swatch: Some(palette.control_bg),
            },
            StyleToken {
                name: "button.accent",
                value: "h56 active bg",
                swatch: Some(palette.control_pressed_bg),
            },
            StyleToken {
                name: "control.select",
                value: "h56 rounded select",
                swatch: Some(palette.control_bg),
            },
            StyleToken {
                name: "setting.row",
                value: "rule + label/value",
                swatch: Some(palette.rule),
            },
        ];

        let layout_tokens = [
            StyleToken {
                name: "window",
                value: "1360 x 860",
                swatch: None,
            },
            StyleToken {
                name: "sidebar",
                value: "340 px",
                swatch: None,
            },
            StyleToken {
                name: "content",
                value: "responsive gap / 132 header",
                swatch: None,
            },
            StyleToken {
                name: "rows",
                value: "nav48/56 setting132",
                swatch: None,
            },
        ];

        let cell_size = format!(
            "{} x {} px",
            self.metrics.cell_size.width, self.metrics.cell_size.height
        );
        let dpi = format!("{} dpi", self.dimensions.dpi);
        let type_tokens = [
            StyleToken {
                name: "ui.font",
                value: "macOS title font",
                swatch: None,
            },
            StyleToken {
                name: "title.font",
                value: "window title font",
                swatch: None,
            },
            StyleToken {
                name: "cell size",
                value: &cell_size,
                swatch: None,
            },
            StyleToken {
                name: "window dpi",
                value: &dpi,
                swatch: None,
            },
        ];

        self.paint_style_group(
            layers,
            notes_x,
            notes_top - scroll,
            notes_width,
            "Component Styles",
            &component_tokens,
        )?;
        self.paint_style_group(
            layers,
            notes_x,
            notes_top + 330.0 - scroll,
            notes_width,
            "Typography",
            &type_tokens,
        )?;
        self.paint_style_group(
            layers,
            notes_x,
            notes_top + 590.0 - scroll,
            notes_width,
            "Layout",
            &layout_tokens,
        )?;

        Ok(())
    }

    fn paint_memory_diagnostics(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = CONTENT_SECTION_Y - scroll;
        let row_count = 11;
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, row_count);
        let card_height = self.settings_card_height(row_count);
        let button_y = card_y + card_height + self.settings_section_card_gap();

        self.draw_text(
            layers,
            &ui_font,
            x,
            section_y,
            "Memory diagnostics are manual. Developer mode only reveals this page; sampling starts when you enable it here.",
            palette.secondary_text,
            max_width,
        )?;

        let snapshot = self
            .ui
            .memory_snapshot
            .clone()
            .unwrap_or_else(|| capture_memory_snapshot(false));
        if self.ui.memory_snapshot.is_none() {
            self.ui.memory_snapshot = Some(snapshot.clone());
        }
        let age_label = format!("{:.1}s ago", snapshot.captured_at.elapsed().as_secs_f32());
        let footprint = snapshot
            .physical_footprint
            .map(format_bytes)
            .unwrap_or_else(|| "Unavailable".to_string());
        let rss = snapshot
            .resident_size
            .map(format_bytes)
            .unwrap_or_else(|| "Unavailable".to_string());
        let peak = snapshot
            .peak_physical_footprint
            .map(format_bytes)
            .unwrap_or_else(|| "Unavailable".to_string());
        let total_resident = snapshot
            .vmmap_total_resident
            .map(format_bytes)
            .unwrap_or_else(|| "Run Refresh Now".to_string());
        let graphics = snapshot
            .vmmap_graphics_resident
            .map(format_bytes)
            .unwrap_or_else(|| "Run Refresh Now".to_string());
        let iosurface = snapshot
            .vmmap_iosurface_resident
            .map(format_bytes)
            .unwrap_or_else(|| "Run Refresh Now".to_string());
        let malloc = snapshot
            .vmmap_malloc_resident
            .map(format_bytes)
            .unwrap_or_else(|| "Run Refresh Now".to_string());
        let text = snapshot
            .vmmap_text_resident
            .map(format_bytes)
            .unwrap_or_else(|| "Run Refresh Now".to_string());
        let breakdown_status = if snapshot.has_vmmap_breakdown() {
            "Captured"
        } else if snapshot.vmmap_error.is_some() {
            "Unavailable"
        } else {
            "Refresh for vmmap"
        };
        let monitor_state = if self.ui.memory_monitoring {
            "Running"
        } else {
            "Off"
        };
        let resource_lines = self.memory_resource_lines();
        let resource_card_y = button_y + CONTROL_HEIGHT + self.settings_section_card_gap();
        let resource_line_height = 30.0;
        let resource_card_height = 88.0 + resource_line_height * resource_lines.len() as f32;
        self.ui.content_scroll.set_extents(
            self.content_bottom(),
            self.settings_content_extent(resource_card_y + scroll + resource_card_height),
        );

        let card_padding = 36.0;
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y,
            row_width,
            "Manual Sampling",
            "Keeps refreshing this page until you turn it off.",
            monitor_state,
            false,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step,
            row_width,
            "Physical Footprint",
            "Matches the macOS memory pressure number more closely than RSS.",
            &footprint,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 2.0,
            row_width,
            "Resident Size",
            "Current resident process memory from proc_pid_rusage.",
            &rss,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 3.0,
            row_width,
            "Peak Physical Footprint",
            "Highest physical footprint reported for this process lifetime.",
            &peak,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 4.0,
            row_width,
            "Last Snapshot",
            "Manual refresh and copy use this latest captured value.",
            &age_label,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 5.0,
            row_width,
            "vmmap Breakdown",
            "Refresh Now captures the slower macOS breakdown; sampling keeps this lightweight.",
            breakdown_status,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 6.0,
            row_width,
            "Activity Monitor Resident",
            "vmmap TOTAL resident; this is the scary-looking number Activity Monitor can resemble.",
            &total_resident,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 7.0,
            row_width,
            "Graphics Surfaces",
            "IOSurface + IOAccelerator graphics + owned unmapped graphics.",
            &graphics,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 8.0,
            row_width,
            "IOSurface",
            "macOS window backing surfaces and swapchain-style drawable storage.",
            &iosurface,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 9.0,
            row_width,
            "Allocator Heap",
            "MALLOC resident from vmmap; Rust allocations mostly land here.",
            &malloc,
            true,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step * 10.0,
            row_width,
            "Text Segments",
            "__TEXT resident pages from the app, dependencies, and system libraries.",
            &text,
            true,
        )?;
        if let Some(error) = &snapshot.vmmap_error {
            self.draw_text(
                layers,
                &ui_font,
                row_x,
                first_row_y + row_step * 11.0,
                error,
                palette.muted_text,
                row_width,
            )?;
        }
        let primary_label = if self.ui.memory_monitoring {
            "Stop Memory Sampling"
        } else {
            "Start Memory Sampling"
        };
        self.draw_button(
            layers,
            x,
            button_y,
            self.button_width_for_label(primary_label, 300.0),
            primary_label,
            SettingsAction::ToggleMemoryMonitoring,
        )?;
        let refresh_x = x + self.button_width_for_label(primary_label, 300.0) + 16.0;
        self.draw_button(
            layers,
            refresh_x,
            button_y,
            self.button_width_for_label("Refresh Now", 210.0),
            "Refresh Now",
            SettingsAction::RefreshMemorySnapshot,
        )?;
        let copied = self
            .ui
            .memory_snapshot_copied_until
            .is_some_and(|until| Instant::now() < until);
        let copy_label = if copied { "Copied" } else { "Copy" };
        let copy_x = refresh_x + self.button_width_for_label("Refresh Now", 210.0) + 16.0;
        self.draw_button(
            layers,
            copy_x,
            button_y,
            self.button_width_for_label(copy_label, 150.0),
            copy_label,
            SettingsAction::CopyMemorySnapshot,
        )?;

        self.paint_group_card(layers, x, resource_card_y, max_width, resource_card_height)?;
        self.draw_text(
            layers,
            &ui_font,
            row_x,
            resource_card_y + 28.0,
            "Renderer Resources",
            palette.title,
            row_width,
        )?;
        for (idx, line) in resource_lines.iter().enumerate() {
            self.draw_text(
                layers,
                &ui_font,
                row_x,
                resource_card_y + 66.0 + resource_line_height * idx as f32,
                line,
                palette.secondary_text,
                row_width,
            )?;
        }

        Ok(())
    }

    fn paint_preview_search(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        self.draw_text(layers, &ui_font, x, y, "Search Field", palette.title, width)?;
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y + 28.0,
            width.min(420.0),
            CONTROL_HEIGHT,
            palette.search_bg,
            palette.search_border,
            CONTROL_RADIUS,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            x + 18.0,
            self.control_text_y(y + 28.0, CONTROL_HEIGHT),
            "Search settings...",
            palette.muted_text,
            width.min(420.0) - 36.0,
        )?;
        Ok(())
    }

    fn paint_preview_sidebar_row(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        selected: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let row_width = width.min(420.0);
        if selected {
            self.draw_rounded_rect(
                layers,
                0,
                x,
                y,
                row_width,
                NAV_ROW_HEIGHT,
                palette.nav_selected_bg,
                NAV_ROW_RADIUS,
            )?;
        } else {
            self.draw_rounded_rect(
                layers,
                0,
                x,
                y,
                row_width,
                NAV_ROW_HEIGHT,
                palette.nav_hover_bg,
                NAV_ROW_RADIUS,
            )?;
        }
        self.draw_text(
            layers,
            &ui_font,
            x + 16.0,
            self.control_text_y(y, NAV_ROW_HEIGHT),
            label,
            if selected {
                palette.selected_text
            } else {
                palette.secondary_text
            },
            row_width - 32.0,
        )?;
        Ok(())
    }

    fn paint_preview_button(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        accent: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let width = width.max(self.button_width_for_label(label, 0.0));
        let bg = if accent {
            palette.nav_selected_bg
        } else {
            palette.control_bg
        };
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            CONTROL_HEIGHT,
            bg,
            palette.control_border,
            CONTROL_RADIUS,
        )?;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            x + 18.0,
            self.control_text_y(y, CONTROL_HEIGHT),
            label,
            if accent {
                palette.selected_text
            } else {
                palette.text
            },
            width - 36.0,
        )?;
        Ok(())
    }

    fn paint_preview_control(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        value: &str,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            CONTROL_HEIGHT,
            palette.control_bg,
            palette.control_border,
            CONTROL_RADIUS,
        )?;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            x + 14.0,
            self.control_text_y(y, CONTROL_HEIGHT),
            value,
            palette.text,
            width - 42.0,
        )?;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            x + width - 28.0,
            self.control_text_y(y, CONTROL_HEIGHT),
            "v",
            palette.muted_text,
            14.0,
        )?;
        Ok(())
    }

    fn paint_style_group(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        title: &str,
        tokens: &[StyleToken<'_>],
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            x,
            y,
            title,
            palette.title,
            width,
        )?;
        self.paint_separator(layers, x, y + 28.0, width)?;

        let mut row_y = y + 58.0;
        for token in tokens {
            self.paint_style_token(
                layers,
                x,
                row_y,
                width,
                token.name,
                token.value,
                token.swatch,
            )?;
            row_y += 40.0;
        }

        Ok(())
    }

    fn paint_style_token(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        name: &str,
        value: &str,
        swatch: Option<LinearRgba>,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let name_x = if let Some(color) = swatch {
            self.draw_rect(layers, 0, x, y + 3.0, 14.0, 14.0, color)?;
            self.draw_rect(layers, 0, x, y + 3.0, 14.0, 1.0, rgba(255, 255, 255, 0.18))?;
            x + 22.0
        } else {
            x
        };
        let value_x = x + width * 0.48;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            name_x,
            y,
            name,
            palette.secondary_text,
            (value_x - name_x - 12.0).max(80.0),
        )?;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            value_x,
            y,
            value,
            palette.text,
            (x + width - value_x).max(80.0),
        )?;
        Ok(())
    }

    fn paint_compatibility(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let scroll = self.ui.content_scroll.offset;
        let row_step = self.settings_row_step();
        let section_y = CONTENT_SECTION_Y - scroll;
        let (card_y, first_row_y) = self.settings_card_geometry(section_y, 2);
        let card_height = self.settings_card_height(2);
        let buttons_y = card_y + card_height + self.settings_section_card_gap();
        self.ui.content_scroll.set_extents(
            self.content_bottom(),
            self.settings_content_extent(buttons_y + scroll + CONTROL_HEIGHT),
        );
        let source = Self::config_source_summary();

        self.draw_text(
            layers,
            &ui_font,
            x,
            section_y,
            "ThinkTerm keeps the WezTerm-compatible config path as the main terminal configuration.",
            palette.secondary_text,
            max_width,
        )?;
        let card_padding = 36.0;
        let row_x = x + card_padding;
        let row_width = max_width - card_padding * 2.0;
        self.paint_group_card(layers, x, card_y, max_width, card_height)?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y,
            row_width,
            "Active Source",
            &source,
            if config::configuration_file().is_some() {
                "File"
            } else {
                "Defaults"
            },
            false,
        )?;
        self.paint_setting_row(
            layers,
            row_x,
            first_row_y + row_step,
            row_width,
            "GUI Editing",
            "Future visual editor will generate a managed compatible config layer.",
            "Pending",
            true,
        )?;
        self.draw_button(
            layers,
            x,
            buttons_y,
            self.button_width_for_label("Open Config File", 260.0),
            "Open Config File",
            SettingsAction::OpenConfigFile,
        )?;
        self.draw_button(
            layers,
            x + 326.0,
            buttons_y,
            self.button_width_for_label("Compatibility Status", 260.0),
            "Compatibility Status",
            SettingsAction::ShowCompatibilityStatus,
        )?;

        Ok(())
    }

    fn paint_placeholder(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        x: f32,
        body: &str,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.ui
            .content_scroll
            .set_extents(self.content_bottom(), 240.0);
        let scroll = self.ui.content_scroll.offset;
        self.draw_text(
            layers,
            font,
            x,
            CONTENT_SECTION_Y - scroll,
            body,
            palette.secondary_text,
            max_width,
        )?;
        self.draw_rect(
            layers,
            0,
            x,
            CONTENT_RULE_Y - scroll,
            max_width,
            1.0,
            palette.rule,
        )?;
        Ok(())
    }

    fn paint_setting_row(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        description: &str,
        value: &str,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - 28.0, width)?;
        }
        let control_width = if width >= 680.0 {
            280.0_f32.min(width * 0.36)
        } else {
            220.0_f32.min(width * 0.44)
        };
        let control_x = x + width - control_width;
        let control_y = y + 4.0;
        let text_width = (control_x - x - 24.0).max(width * 0.45);
        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            description,
            palette.secondary_text,
            text_width,
        )?;
        self.draw_rounded_frame(
            layers,
            0,
            control_x,
            control_y,
            control_width,
            CONTROL_HEIGHT,
            palette.control_bg,
            palette.control_border,
            CONTROL_RADIUS,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            control_x + 14.0,
            self.control_text_y(control_y, CONTROL_HEIGHT),
            value,
            palette.text,
            control_width - 26.0,
        )?;
        Ok(())
    }

    fn paint_toggle_setting_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        description: &str,
        enabled: bool,
        action: SettingsAction,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - 28.0, width)?;
        }
        let control_width = if width >= 680.0 {
            280.0_f32.min(width * 0.36)
        } else {
            220.0_f32.min(width * 0.44)
        };
        let control_x = x + width - control_width;
        let control_y = y + 4.0;
        let text_width = (control_x - x - 24.0).max(width * 0.45);
        let control_rect = rect(control_x, control_y, control_width, CONTROL_HEIGHT);
        self.ui_context
            .push(control_rect, WidgetKind::Button, action);

        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let bg = if pressed {
            palette.control_pressed_bg
        } else if hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if hovered || pressed {
            palette.separator
        } else {
            palette.control_border
        };

        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            description,
            palette.secondary_text,
            text_width,
        )?;
        self.draw_rounded_frame(
            layers,
            0,
            control_x,
            control_y,
            control_width,
            CONTROL_HEIGHT,
            bg,
            border,
            CONTROL_RADIUS,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            control_x + 14.0,
            self.control_text_y(control_y, CONTROL_HEIGHT),
            if enabled { "On" } else { "Off" },
            palette.text,
            control_width - 26.0,
        )?;
        Ok(())
    }

    fn paint_group_card(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            height,
            palette.card_bg,
            palette.separator,
            28.0,
        )
    }

    fn paint_separator(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.draw_rounded_rect(layers, 0, x, y, width, 2.0, palette.rule, 1.0)
    }

    fn paint_font_size_stepper_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        description: &str,
        value: f64,
        reset_action: SettingsAction,
        decrease_action: SettingsAction,
        increase_action: SettingsAction,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - 28.0, width)?;
        }

        let control_width = if width >= 680.0 {
            280.0_f32.min(width * 0.36)
        } else {
            220.0_f32.min(width * 0.44)
        };
        let control_x = x + width - control_width;
        let control_y = y + 4.0;
        let dynamic_icon_size = (self.metrics.cell_size.height as f32 + 4.0).clamp(24.0, 34.0);
        let reset_size = CONTROL_HEIGHT;
        let reset_gap = 14.0;
        let reset_x = (control_x - reset_gap - reset_size).max(x + width * 0.62);
        let text_width = (reset_x - x - 24.0).max(width * 0.40);

        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            description,
            palette.secondary_text,
            text_width,
        )?;

        self.paint_icon_button(
            layers,
            reset_x,
            control_y,
            reset_size,
            SvgIcon::RotateCcw,
            reset_action,
        )?;

        self.draw_rounded_frame(
            layers,
            0,
            control_x,
            control_y,
            control_width,
            CONTROL_HEIGHT,
            palette.control_bg,
            palette.control_border,
            CONTROL_RADIUS,
        )?;

        let button_width = 62.0_f32.min(control_width * 0.28);
        let minus_rect = rect(control_x, control_y, button_width, CONTROL_HEIGHT);
        let plus_rect = rect(
            control_x + control_width - button_width,
            control_y,
            button_width,
            CONTROL_HEIGHT,
        );
        self.ui_context
            .push(minus_rect, WidgetKind::Button, decrease_action);
        self.ui_context
            .push(plus_rect, WidgetKind::Button, increase_action);

        for (button_rect, action, icon) in [
            (minus_rect, decrease_action, SvgIcon::Minus),
            (plus_rect, increase_action, SvgIcon::Plus),
        ] {
            if self.ui.interaction.hovered == Some(action)
                || self.ui.interaction.pressed == Some(action)
            {
                self.draw_rounded_rect(
                    layers,
                    1,
                    button_rect.origin.x + 4.0,
                    button_rect.origin.y + 4.0,
                    button_rect.size.width - 8.0,
                    button_rect.size.height - 8.0,
                    palette.control_hover_bg,
                    CONTROL_RADIUS - 4.0,
                )?;
            }
            self.draw_svg_icon(
                layers,
                icon,
                button_rect.origin.x + (button_rect.size.width - dynamic_icon_size) / 2.0,
                button_rect.origin.y + (button_rect.size.height - dynamic_icon_size) / 2.0,
                dynamic_icon_size,
                palette.secondary_text,
            )?;
        }

        let value_label = if value >= 100.0 && value.fract().abs() < f64::EPSILON {
            format!("{value:.0}")
        } else {
            format!("{value:.1}")
        };
        let value_width = control_width - button_width * 2.0;
        let value_x = control_x + button_width;
        self.draw_text(
            layers,
            &ui_font,
            value_x + 8.0,
            self.control_text_y(control_y, CONTROL_HEIGHT),
            &value_label,
            palette.text,
            value_width - 16.0,
        )?;

        Ok(())
    }

    fn paint_text_setting_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        description: &str,
        value: &str,
        placeholder: &str,
        action: SettingsAction,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - 28.0, width)?;
        }

        let control_width = if width >= 680.0 {
            280.0_f32.min(width * 0.36)
        } else {
            220.0_f32.min(width * 0.44)
        };
        let control_x = x + width - control_width;
        let control_y = y + 4.0;
        let text_width = (control_x - x - 24.0).max(width * 0.45);
        let focused = self.ui.interaction.focused == Some(action);
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let bg = if pressed {
            palette.control_pressed_bg
        } else if focused || hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if focused {
            palette.nav_selected_bg
        } else if hovered || pressed {
            palette.separator
        } else {
            palette.control_border
        };
        let control_rect = rect(control_x, control_y, control_width, CONTROL_HEIGHT);

        self.ui_context
            .push(control_rect, WidgetKind::TextInput, action);
        self.draw_text(layers, &ui_font, x, y, label, palette.text, text_width)?;
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            description,
            palette.secondary_text,
            text_width,
        )?;
        self.draw_rounded_frame(
            layers,
            0,
            control_x,
            control_y,
            control_width,
            CONTROL_HEIGHT,
            bg,
            border,
            CONTROL_RADIUS,
        )?;

        let display = if value.trim().is_empty() {
            placeholder
        } else {
            value
        };
        let text_color = if value.trim().is_empty() {
            palette.muted_text
        } else {
            palette.text
        };
        let selected_all = match action {
            SettingsAction::FontFamilyInput => self.ui.font_family_input.selected_all,
            _ => false,
        };
        if focused && selected_all && !value.is_empty() {
            self.draw_rounded_rect(
                layers,
                1,
                control_x + 10.0,
                control_y + 8.0,
                control_width - 20.0,
                CONTROL_HEIGHT - 16.0,
                palette.nav_selected_bg.mul_alpha(0.32),
                CONTROL_RADIUS - 4.0,
            )?;
        }
        self.draw_text(
            layers,
            &ui_font,
            control_x + 16.0,
            self.control_text_y(control_y, CONTROL_HEIGHT),
            display,
            text_color,
            control_width - 32.0,
        )?;
        if focused && !selected_all {
            let caret_text = if value.trim().is_empty() { "" } else { value };
            let caret_x = control_x
                + 18.0
                + self
                    .measure_text_width(&ui_font, caret_text)
                    .min(control_width - 42.0);
            self.draw_rect(
                layers,
                1,
                caret_x,
                control_y + 12.0,
                1.5,
                CONTROL_HEIGHT - 24.0,
                palette.nav_selected_bg,
            )?;
        }
        Ok(())
    }

    fn paint_theme_mode_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        description: &str,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - 28.0, width)?;
        }

        let (control_x, control_y, control_width) = self.theme_mode_control_geometry(x, y, width);
        let text_width = (control_x - x - 24.0).max(width * 0.45);
        let action = SettingsAction::ToggleThemeModeMenu;
        let control_rect = rect(control_x, control_y, control_width, CONTROL_HEIGHT);
        let open = self.ui.open_dropdown == Some(SettingsDropdown::ThemeMode);
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let bg = if pressed || hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if open {
            palette.nav_selected_bg
        } else if hovered || pressed {
            palette.separator
        } else {
            palette.control_border
        };

        self.ui_context
            .push(control_rect, WidgetKind::Button, action);
        self.draw_text(
            layers,
            &ui_font,
            x,
            y,
            "Theme Mode",
            palette.text,
            text_width,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            description,
            palette.secondary_text,
            text_width,
        )?;
        self.draw_rounded_frame(
            layers,
            0,
            control_rect.origin.x,
            control_rect.origin.y,
            control_rect.size.width,
            control_rect.size.height,
            bg,
            border,
            CONTROL_RADIUS,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            control_x + 16.0,
            self.control_text_y(control_y, CONTROL_HEIGHT),
            self.native_settings.appearance.theme_mode.label(),
            palette.text,
            control_width - 60.0,
        )?;
        self.draw_svg_icon(
            layers,
            SvgIcon::ChevronDown,
            control_x + control_width - 38.0,
            control_y + (CONTROL_HEIGHT - 22.0) / 2.0,
            22.0,
            palette.secondary_text,
        )?;

        Ok(())
    }

    fn paint_app_icon_row(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        description: &str,
        draw_top_rule: bool,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        if draw_top_rule {
            self.paint_separator(layers, x, y - 28.0, width)?;
        }

        let (control_x, control_y, control_width) = self.dropdown_control_geometry(x, y, width);
        let text_width = (control_x - x - 24.0).max(width * 0.45);
        let action = SettingsAction::ToggleAppIconMenu;
        let control_rect = rect(control_x, control_y, control_width, CONTROL_HEIGHT);
        let open = self.ui.open_dropdown == Some(SettingsDropdown::AppIcon);
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let bg = if pressed || hovered {
            palette.control_hover_bg
        } else {
            palette.control_bg
        };
        let border = if open {
            palette.nav_selected_bg
        } else if hovered || pressed {
            palette.separator
        } else {
            palette.control_border
        };

        self.ui_context
            .push(control_rect, WidgetKind::Button, action);
        self.draw_text(layers, &ui_font, x, y, "App Icon", palette.text, text_width)?;
        self.draw_text(
            layers,
            &ui_font,
            x,
            self.settings_row_description_y(y),
            description,
            palette.secondary_text,
            text_width,
        )?;
        self.draw_rounded_frame(
            layers,
            0,
            control_rect.origin.x,
            control_rect.origin.y,
            control_rect.size.width,
            control_rect.size.height,
            bg,
            border,
            CONTROL_RADIUS,
        )?;
        self.draw_text(
            layers,
            &ui_font,
            control_x + 16.0,
            self.control_text_y(control_y, CONTROL_HEIGHT),
            self.native_settings.appearance.app_icon.label(),
            palette.text,
            control_width - 60.0,
        )?;
        self.draw_svg_icon(
            layers,
            SvgIcon::ChevronDown,
            control_x + control_width - 38.0,
            control_y + (CONTROL_HEIGHT - 22.0) / 2.0,
            22.0,
            palette.secondary_text,
        )?;

        Ok(())
    }

    fn theme_mode_control_geometry(&self, x: f32, y: f32, width: f32) -> (f32, f32, f32) {
        self.dropdown_control_geometry(x, y, width)
    }

    fn dropdown_control_geometry(&self, x: f32, y: f32, width: f32) -> (f32, f32, f32) {
        let control_width = if width >= 680.0 {
            280.0_f32.min(width * 0.36)
        } else {
            220.0_f32.min(width * 0.44)
        };
        (x + width - control_width, y + 4.0, control_width)
    }

    fn paint_open_dropdown_overlay(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        max_width: f32,
    ) -> anyhow::Result<()> {
        let Some(dropdown) = self.ui.open_dropdown else {
            return Ok(());
        };

        let scroll = self.ui.content_scroll.offset;
        let card_padding = 36.0;
        let section_y = CONTENT_SECTION_Y - scroll;
        let (_, first_row_y) = self.settings_card_geometry(section_y, 4);
        let (row_x, row_y, row_width) = match self.selected {
            SettingsSection::Appearance => {
                let row_y = match dropdown {
                    SettingsDropdown::ThemeMode => first_row_y,
                    SettingsDropdown::AppIcon => first_row_y + self.settings_row_step(),
                };
                (x + card_padding, row_y, max_width - card_padding * 2.0)
            }
            SettingsSection::General if dropdown == SettingsDropdown::ThemeMode => (
                x + card_padding,
                first_row_y + self.settings_row_step() * 2.0,
                max_width - card_padding * 2.0,
            ),
            _ => return Ok(()),
        };
        let (control_x, control_y, control_width) =
            self.dropdown_control_geometry(row_x, row_y, row_width);
        match dropdown {
            SettingsDropdown::ThemeMode => self.paint_theme_mode_menu(
                layers,
                control_x,
                control_y + CONTROL_HEIGHT + 8.0,
                control_width,
            ),
            SettingsDropdown::AppIcon => self.paint_app_icon_menu(
                layers,
                control_x,
                control_y + CONTROL_HEIGHT + 8.0,
                control_width,
            ),
        }
    }

    fn paint_theme_mode_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let options = [
            (
                NativeThemeMode::System.label(),
                SettingsAction::SetThemeMode(NativeThemeMode::System),
                self.native_settings.appearance.theme_mode == NativeThemeMode::System,
            ),
            (
                NativeThemeMode::Light.label(),
                SettingsAction::SetThemeMode(NativeThemeMode::Light),
                self.native_settings.appearance.theme_mode == NativeThemeMode::Light,
            ),
            (
                NativeThemeMode::Dark.label(),
                SettingsAction::SetThemeMode(NativeThemeMode::Dark),
                self.native_settings.appearance.theme_mode == NativeThemeMode::Dark,
            ),
        ];
        self.paint_dropdown_menu(layers, x, y, width, &options)
    }

    fn paint_app_icon_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
    ) -> anyhow::Result<()> {
        let options = [
            (
                NativeAppIcon::Default.label(),
                SettingsAction::SetAppIcon(NativeAppIcon::Default),
                self.native_settings.appearance.app_icon == NativeAppIcon::Default,
            ),
            (
                NativeAppIcon::Simple.label(),
                SettingsAction::SetAppIcon(NativeAppIcon::Simple),
                self.native_settings.appearance.app_icon == NativeAppIcon::Simple,
            ),
        ];
        self.paint_dropdown_menu(layers, x, y, width, &options)
    }

    fn paint_dropdown_menu(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        options: &[(&'static str, SettingsAction, bool)],
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let ui_font = Rc::clone(&self.ui_font);
        let row_height = 46.0;
        let row_gap = 6.0;
        let menu_padding = 8.0;
        let menu_height = menu_padding * 2.0
            + row_height * options.len() as f32
            + row_gap * options.len().saturating_sub(1) as f32;
        let menu_bg = match self.effective_appearance() {
            Appearance::Light | Appearance::LightHighContrast => rgba(248, 248, 250, 1.0),
            Appearance::Dark | Appearance::DarkHighContrast => rgba(34, 34, 36, 1.0),
        };

        self.draw_rounded_frame(
            layers,
            1,
            x,
            y,
            width,
            menu_height,
            menu_bg,
            palette.control_border,
            CONTROL_RADIUS,
        )?;

        let mut row_y = y + menu_padding;
        for (label, action, selected) in options.iter().copied() {
            let row_rect = rect(x + 8.0, row_y, width - 16.0, row_height);
            self.ui_context.push(row_rect, WidgetKind::Button, action);
            let hovered = self.ui.interaction.hovered == Some(action);
            let pressed = self.ui.interaction.pressed == Some(action);
            let row_bg = if selected {
                Some(palette.nav_selected_bg)
            } else if pressed {
                Some(palette.control_pressed_bg)
            } else if hovered {
                Some(palette.control_hover_bg)
            } else {
                None
            };
            if let Some(row_bg) = row_bg {
                self.draw_rounded_rect(
                    layers,
                    1,
                    row_rect.origin.x,
                    row_rect.origin.y,
                    row_rect.size.width,
                    row_rect.size.height,
                    row_bg,
                    9.0,
                )?;
            }
            self.draw_text(
                layers,
                &ui_font,
                row_rect.origin.x + 14.0,
                self.control_text_y(row_rect.origin.y, row_height),
                label,
                if selected {
                    palette.selected_text
                } else {
                    palette.text
                },
                row_rect.size.width - 28.0,
            )?;
            row_y += row_height + row_gap;
        }

        Ok(())
    }

    fn content_bottom(&self) -> f32 {
        self.dimensions.pixel_height as f32
    }

    fn sidebar_scrollbar_visible(&self) -> bool {
        self.ui
            .sidebar_scrollbar_visible_until
            .is_some_and(|until| until > Instant::now())
            || matches!(self.ui.drag, Some(SettingsDrag::SidebarResize { .. }))
    }

    fn content_scrollbar_visible(&self) -> bool {
        self.ui
            .content_scrollbar_visible_until
            .is_some_and(|until| until > Instant::now())
    }

    fn filtered_sections(&self) -> Vec<SettingsSection> {
        let query = self.ui.search.text.trim().to_lowercase();
        let sections = self.visible_sections();
        if query.is_empty() {
            return sections;
        }
        sections
            .into_iter()
            .filter(|section| {
                section.label().to_lowercase().contains(&query)
                    || section
                        .search_terms()
                        .iter()
                        .any(|term| term.to_lowercase().contains(&query))
            })
            .collect()
    }

    fn paint_text_input(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        spec: TextInputSpec<'_, SettingsAction>,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        self.ui_context
            .push(spec.rect, WidgetKind::TextInput, spec.action);
        let border = if spec.focused {
            palette.nav_selected_bg
        } else if self.ui.interaction.hovered == Some(spec.action) {
            palette.separator
        } else {
            palette.search_border
        };
        self.draw_rounded_frame(
            layers,
            0,
            spec.rect.origin.x,
            spec.rect.origin.y,
            spec.rect.size.width,
            spec.rect.size.height,
            palette.search_bg,
            border,
            self.ui.tokens.control_radius,
        )?;
        if spec.focused {
            self.draw_rounded_rect(
                layers,
                0,
                spec.rect.origin.x,
                spec.rect.origin.y,
                spec.rect.size.width,
                2.0,
                palette.nav_selected_bg,
                1.0,
            )?;
        }

        let text = if spec.text.is_empty() && !spec.focused {
            spec.placeholder
        } else {
            spec.text
        };
        let color = if spec.text.is_empty() && !spec.focused {
            palette.muted_text
        } else {
            palette.text
        };
        if spec.focused && spec.selected_all && !spec.text.is_empty() {
            self.draw_rounded_rect(
                layers,
                1,
                spec.rect.origin.x + 50.0,
                spec.rect.origin.y + 8.0,
                spec.rect.size.width - 96.0,
                spec.rect.size.height - 16.0,
                palette.nav_selected_bg.mul_alpha(0.32),
                self.ui.tokens.control_radius - 4.0,
            )?;
        }
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            spec.rect.origin.x + 58.0,
            self.control_text_y(spec.rect.origin.y, spec.rect.size.height),
            text,
            color,
            spec.rect.size.width - 116.0,
        )?;
        if spec.focused && !spec.selected_all {
            let caret_x = spec.rect.origin.x
                + 60.0
                + self
                    .measure_text_width(&Rc::clone(&self.ui_font), spec.text)
                    .min((spec.rect.size.width - 124.0).max(0.0));
            self.draw_rect(
                layers,
                1,
                caret_x,
                spec.rect.origin.y + 10.0,
                1.5,
                spec.rect.size.height - 20.0,
                palette.nav_selected_bg,
            )?;
        }
        Ok(())
    }

    fn paint_scrollbar(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        area: window::RectF,
        scroll: ScrollState,
        visible: bool,
    ) -> anyhow::Result<()> {
        if !visible {
            return Ok(());
        }
        let ui_palette = UiPalette::for_appearance(self.effective_appearance());
        let spec = ScrollbarSpec::from_area(area, self.ui.tokens);
        if let Some((thumb_y, thumb_h)) = scroll.thumb(spec.y, spec.height) {
            self.draw_rounded_rect(
                layers,
                0,
                spec.x,
                thumb_y,
                spec.width,
                thumb_h,
                ui_palette.scrollbar_thumb,
                spec.width / 2.0,
            )?;
        }
        Ok(())
    }

    fn draw_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        width: f32,
        label: &str,
        action: SettingsAction,
    ) -> anyhow::Result<()> {
        let width = width.max(self.button_width_for_label(label, 0.0));
        let button = ButtonSpec {
            label,
            action,
            rect: rect(x, y, width, CONTROL_HEIGHT),
            state: if self.ui.interaction.pressed == Some(action) {
                ControlState::Pressed
            } else if self.ui.interaction.hovered == Some(action) {
                ControlState::Hovered
            } else {
                ControlState::Normal
            },
            kind: WidgetKind::Button,
        };
        self.ui_context
            .push(button.rect, button.kind, button.action);

        let palette = self.palette();
        let ui_palette = UiPalette::for_appearance(self.effective_appearance());
        let (background, border) = button.state.colors(ui_palette);
        self.draw_rounded_frame(
            layers,
            0,
            x,
            y,
            width,
            CONTROL_HEIGHT,
            background,
            border,
            CONTROL_RADIUS,
        )?;
        self.draw_text(
            layers,
            &Rc::clone(&self.ui_font),
            x + 14.0,
            self.control_text_y(y, CONTROL_HEIGHT),
            button.label,
            palette.text,
            width - 36.0,
        )?;
        Ok(())
    }

    fn paint_icon_button(
        &mut self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        x: f32,
        y: f32,
        size: f32,
        icon: SvgIcon,
        action: SettingsAction,
    ) -> anyhow::Result<()> {
        let palette = self.palette();
        let rect = rect(x, y, size, size);
        self.ui_context.push(rect, WidgetKind::Button, action);
        let hovered = self.ui.interaction.hovered == Some(action);
        let pressed = self.ui.interaction.pressed == Some(action);
        let bg = if pressed {
            palette.control_pressed_bg
        } else if hovered {
            palette.control_hover_bg
        } else {
            LinearRgba::TRANSPARENT
        };
        if bg.3 > 0.0 {
            self.draw_rounded_rect(layers, 0, x, y, size, size, bg, CONTROL_RADIUS)?;
        }
        let icon_size = (self.metrics.cell_size.height as f32 + 4.0).clamp(24.0, 34.0);
        self.draw_svg_icon(
            layers,
            icon,
            x + (size - icon_size) / 2.0,
            y + (size - icon_size) / 2.0,
            icon_size,
            if hovered || pressed {
                palette.text
            } else {
                palette.muted_text
            },
        )
    }

    fn button_width_for_label(&self, label: &str, min_width: f32) -> f32 {
        (self.measure_text_width(&Rc::clone(&self.ui_font), label) + 44.0).max(min_width)
    }

    fn control_text_y(&self, y: f32, height: f32) -> f32 {
        let cell_height = self.metrics.cell_size.height as f32;
        y + ((height - cell_height) / 2.0).max(0.0)
    }

    fn draw_rounded_frame(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        fill: LinearRgba,
        border: LinearRgba,
        radius: f32,
    ) -> anyhow::Result<()> {
        self.draw_rounded_rect(layers, layer_num, x, y, width, height, border, radius)?;
        self.draw_rounded_rect(
            layers,
            layer_num,
            x + 1.0,
            y + 1.0,
            width - 2.0,
            height - 2.0,
            fill,
            (radius - 1.0).max(0.0),
        )?;
        Ok(())
    }

    fn draw_rounded_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: LinearRgba,
        radius: f32,
    ) -> anyhow::Result<()> {
        if width <= 0.0 || height <= 0.0 {
            return Ok(());
        }

        let radius = radius.min(width / 2.0).min(height / 2.0).round().max(0.0);
        if radius <= 0.0 {
            return self.draw_rect(layers, layer_num, x, y, width, height, color);
        }

        let corner_size = euclid::size2(radius, radius);
        self.draw_corner(
            layers,
            layer_num,
            x,
            y,
            TOP_LEFT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;
        self.draw_corner(
            layers,
            layer_num,
            x + width - radius,
            y,
            TOP_RIGHT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;
        self.draw_corner(
            layers,
            layer_num,
            x,
            y + height - radius,
            BOTTOM_LEFT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;
        self.draw_corner(
            layers,
            layer_num,
            x + width - radius,
            y + height - radius,
            BOTTOM_RIGHT_ROUNDED_CORNER,
            corner_size,
            color,
        )?;

        self.draw_rect(
            layers,
            layer_num,
            x + radius,
            y,
            width - radius * 2.0,
            height,
            color,
        )?;
        self.draw_rect(
            layers,
            layer_num,
            x,
            y + radius,
            radius,
            height - radius * 2.0,
            color,
        )?;
        self.draw_rect(
            layers,
            layer_num,
            x + width - radius,
            y + radius,
            radius,
            height - radius * 2.0,
            color,
        )?;

        Ok(())
    }

    fn draw_corner(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        polys: &'static [Poly],
        size: euclid::Size2D<f32, window::PixelUnit>,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        let render_state = self.render_state.as_ref().unwrap();
        let sprite = render_state
            .glyph_cache
            .borrow_mut()
            .cached_block(
                BlockKey::PolyWithCustomMetrics {
                    polys,
                    underline_height: self.metrics.underline_height,
                    cell_size: euclid::size2(size.width as isize, size.height as isize),
                },
                &self.metrics,
            )?
            .texture_coords();

        let mut quad = layers.allocate(layer_num)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + size.width - left_offset,
            y + size.height - top_offset,
        );
        quad.set_texture(sprite);
        quad.set_fg_color(color);
        quad.set_alt_color_and_mix_value(color, 0.0);
        quad.set_hsv(None);
        quad.set_has_color(false);
        quad.set_grayscale();
        Ok(())
    }

    fn draw_rect(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        layer_num: usize,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        if width <= 0.0 || height <= 0.0 {
            return Ok(());
        }

        let render_state = self.render_state.as_ref().unwrap();
        let mut quad = layers.allocate(layer_num)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + width - left_offset,
            y + height - top_offset,
        );
        quad.set_texture(render_state.util_sprites.filled_box.texture_coords());
        quad.set_is_background();
        quad.set_fg_color(color);
        quad.set_hsv(None);
        Ok(())
    }

    fn draw_svg_icon(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        icon: SvgIcon,
        x: f32,
        y: f32,
        size: f32,
        color: LinearRgba,
    ) -> anyhow::Result<()> {
        if size <= 0.0 {
            return Ok(());
        }

        let render_state = self.render_state.as_ref().unwrap();
        let sprite = render_state
            .glyph_cache
            .borrow_mut()
            .cached_svg_icon(icon, size.round() as usize)?
            .texture_coords();
        let mut quad = layers.allocate(2)?;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        quad.set_position(
            x - left_offset,
            y - top_offset,
            x + size - left_offset,
            y + size - top_offset,
        );
        quad.set_texture(sprite);
        quad.set_fg_color(color);
        quad.set_alt_color_and_mix_value(color, 0.0);
        quad.set_hsv(None);
        quad.set_has_color(false);
        quad.set_grayscale();
        Ok(())
    }

    fn draw_text(
        &self,
        layers: &mut TripleLayerQuadAllocator<'_>,
        font: &Rc<LoadedFont>,
        x: f32,
        y: f32,
        text: &str,
        color: LinearRgba,
        max_width: f32,
    ) -> anyhow::Result<()> {
        if text.is_empty() || max_width <= 0.0 {
            return Ok(());
        }

        let display_text = self.text_with_ellipsis(font, text, max_width);
        if display_text.is_empty() {
            return Ok(());
        }

        let infos = font.blocking_shape(&display_text, None, Direction::LeftToRight, None, None)?;
        let render_state = self.render_state.as_ref().unwrap();
        let mut glyph_cache = render_state.glyph_cache.borrow_mut();
        let style = font.style();
        let mut pos_x = x;
        let baseline = self.metrics.cell_size.height as f32 + self.metrics.descender.get() as f32;
        let left_offset = self.dimensions.pixel_width as f32 / 2.0;
        let top_offset = self.dimensions.pixel_height as f32 / 2.0;
        let right_edge = x + max_width;

        for info in infos {
            let glyph = glyph_cache.cached_glyph(&info, style, false, font, &self.metrics, 1)?;
            if let Some(texture) = glyph.texture.as_ref() {
                let glyph_x = (pos_x + (glyph.x_offset + glyph.bearing_x).get() as f32).round();
                let glyph_y =
                    (y - (glyph.y_offset + glyph.bearing_y).get() as f32 + baseline).round();
                let width = texture.coords.size.width as f32 * glyph.scale as f32;
                let height = texture.coords.size.height as f32 * glyph.scale as f32;

                if glyph_x + width > right_edge {
                    break;
                }

                let mut quad = layers.allocate(1)?;
                quad.set_position(
                    glyph_x - left_offset,
                    glyph_y - top_offset,
                    glyph_x + width - left_offset,
                    glyph_y + height - top_offset,
                );
                quad.set_texture(texture.texture_coords());
                quad.set_has_color(glyph.has_color);
                quad.set_fg_color(color);
                quad.set_hsv(None);
            }
            pos_x += glyph.x_advance.get() as f32;
            if pos_x > right_edge {
                break;
            }
        }

        Ok(())
    }

    fn measure_text_width(&self, font: &Rc<LoadedFont>, text: &str) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        let Ok(infos) = font.blocking_shape(text, None, Direction::LeftToRight, None, None) else {
            return 0.0;
        };
        let render_state = self.render_state.as_ref().unwrap();
        let mut glyph_cache = render_state.glyph_cache.borrow_mut();
        let style = font.style();
        infos
            .into_iter()
            .filter_map(|info| {
                glyph_cache
                    .cached_glyph(&info, style, false, font, &self.metrics, 1)
                    .ok()
                    .map(|glyph| glyph.x_advance.get() as f32)
            })
            .sum()
    }

    fn text_with_ellipsis(&self, font: &Rc<LoadedFont>, text: &str, max_width: f32) -> String {
        if max_width <= 0.0 || text.is_empty() {
            return String::new();
        }

        if self.measure_text_width(font, text) <= max_width {
            return text.to_string();
        }

        let ellipsis = "...";
        let ellipsis_width = self.measure_text_width(font, ellipsis);
        if ellipsis_width > max_width {
            return String::new();
        }

        let mut boundaries = text
            .char_indices()
            .map(|(idx, _)| idx)
            .chain(std::iter::once(text.len()))
            .collect::<Vec<_>>();
        boundaries.dedup();

        let mut low = 0;
        let mut high = boundaries.len().saturating_sub(1);
        let mut best = 0;
        while low <= high {
            let mid = (low + high) / 2;
            let candidate = &text[..boundaries[mid]];
            let width = self.measure_text_width(font, candidate) + ellipsis_width;
            if width <= max_width {
                best = mid;
                low = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                high = mid - 1;
            }
        }

        format!("{}{}", text[..boundaries[best]].trim_end(), ellipsis)
    }

    fn native_settings_path() -> PathBuf {
        crate::native_settings::settings_path()
    }

    fn load_native_settings() -> ThinkTermNativeSettings {
        crate::native_settings::load()
    }

    fn config_source_summary() -> String {
        if let Some(path) = config::configuration_file() {
            format!("Loaded from {}", path.display())
        } else if config::is_config_overridden() {
            "No config file loaded; command-line overrides or --skip-config are active".to_string()
        } else {
            "No config file loaded; using built-in defaults".to_string()
        }
    }

    fn effective_color_scheme_label(config: &config::ConfigHandle) -> &str {
        if let Some(name) = config.color_scheme.as_deref() {
            name
        } else if config.colors.is_some() {
            "Custom inline colors"
        } else {
            "Default palette"
        }
    }

    fn effective_font_family(config: &config::ConfigHandle) -> String {
        config
            .font
            .font
            .first()
            .map(|font| font.family.clone())
            .unwrap_or_else(|| "Default font".to_string())
    }

    fn initial_status() -> String {
        Self::config_source_summary()
    }

    fn open_path(path: PathBuf) {
        match url::Url::from_file_path(&path) {
            Ok(url) => wezterm_open_url::open_url(url.as_str()),
            Err(_) => log::error!("Unable to convert {} into a file URL", path.display()),
        }
    }
}

fn rgba(r: u8, g: u8, b: u8, a: f32) -> LinearRgba {
    let mut color = LinearRgba::with_srgba(r, g, b, 255);
    color.3 = a;
    color
}

fn wgpu_color(color: LinearRgba) -> wgpu::Color {
    wgpu::Color {
        r: color.0 as f64,
        g: color.1 as f64,
        b: color.2 as f64,
        a: color.3 as f64,
    }
}
