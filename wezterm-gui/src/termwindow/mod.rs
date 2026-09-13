#![allow(clippy::range_plus_one)]
use super::renderstate::*;
use super::utilsprites::RenderMetrics;
use crate::colorease::ColorEase;
use crate::frontend::{front_end, try_front_end};
use crate::inputmap::InputMap;
use crate::overlay::{
    confirm_close_pane, confirm_close_tab, confirm_close_window, confirm_quit_program, launcher,
    start_overlay, start_overlay_pane, CopyModeParams, CopyOverlay, LauncherArgs, LauncherFlags,
    QuickSelectOverlay,
};
use crate::quad::{HeapQuadAllocator, HeapQuadMark};
use crate::resize_increment_calculator::ResizeIncrementCalculator;
use crate::scripting::guiwin::GuiWin;
use crate::scrollbar::*;
use crate::selection::Selection;
use crate::shapecache::*;
use crate::tabbar::{TabBarItem, TabBarState};
use crate::termwindow::background::{
    load_background_image, reload_background_image, LoadedBackgroundLayer,
};
use crate::termwindow::content_view::{ContentView, ContentViewId, ContentViewPresentation};
use crate::termwindow::keyevent::{KeyTableArgs, KeyTableState};
use crate::termwindow::modal::Modal;
use crate::termwindow::render::paint::AllowImage;
use crate::termwindow::render::{
    CachedLineState, LineQuadCacheKey, LineQuadCacheValue, LineToEleShapeCacheKey,
    LineToElementShapeItem,
};
use crate::termwindow::webgpu::WebGpuState;
use crate::ui::TextInputState;
use ::wezterm_term::input::{ClickPosition, MouseButton as TMB};
use ::window::color::LinearRgba;
use ::window::*;
use anyhow::{anyhow, ensure, Context};
use config::keyassignment::{
    ClipboardPasteSource, Confirmation, KeyAssignment, LauncherActionArgs, PaneDirection, Pattern,
    PromptInputLine, QuickSelectArguments, RotationDirection, SpawnCommand, SplitSize,
};
use config::window::WindowLevel;
use config::{
    configuration, AudibleBell, ConfigHandle, Dimension, DimensionContext, GeometryOrigin,
    GuiPosition, RgbaColor, TabBarColor, TabBarColors, TermConfig, WindowCloseConfirmation,
};
use lfucache::*;
use mlua::{FromLua, LuaSerdeExt, UserData, UserDataFields};
use mux::domain::DomainId;
use mux::pane::{
    CachePolicy, CloseReason, Pane, PaneId, Pattern as MuxPattern, PerformAssignmentResult,
};
use mux::renderable::RenderableDimensions;
use mux::tab::{
    CollapsedPaneLayout, PaneStackId, PositionedPane, PositionedSplit, SplitDirection,
    SplitRequest, SplitSize as MuxSplitSize, Tab, TabId,
};
use mux::window::WindowId as MuxWindowId;
use mux::{Mux, MuxNotification};
use mux_lua::MuxPane;
use smol::channel::Sender;
use smol::Timer;
use std::cell::{Cell, RefCell, RefMut};
use std::collections::{HashMap, HashSet, LinkedList, VecDeque};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use termwiz::hyperlink::Hyperlink;
use termwiz::image::ImageData;
use termwiz::surface::SequenceNo;
use wezterm_client::domain::{ClientDomain, FrontendRecoverySlot};
use wezterm_dynamic::Value;
use wezterm_font::FontConfiguration;
use wezterm_term::color::{ColorAttribute, ColorPalette};
use wezterm_term::input::LastMouseClick;
use wezterm_term::{Alert, Progress, StableRowIndex, TerminalConfiguration, TerminalSize};

/// A domain-owned remote window should stay in the local mux when its native
/// GUI window closes.  The origin tag is authoritative; the Space/domain
/// fallback covers a window created before that tag was available, but only
/// when every pane in the actual mux window belongs to that same domain.
/// Requiring all panes avoids treating a local Space containing one manually
/// opened remote tab as a remote-owned window.
fn preserve_mux_window_on_gui_close(
    origin_client_domain: Option<DomainId>,
    active_space_domain: Option<DomainId>,
    pane_domains: &[DomainId],
    client_pane_domains: &[DomainId],
) -> bool {
    if let Some(domain_id) = origin_client_domain {
        // A tag identifies ownership, but an empty/tag-only connection window
        // has no remote session to preserve.  Requiring a real ClientPane is
        // also what lets close cancel an initial failed attach.
        return client_pane_domains
            .iter()
            .any(|pane_domain| *pane_domain == domain_id);
    }

    active_space_domain.is_some_and(|domain_id| {
        !pane_domains.is_empty()
            && pane_domains.len() == client_pane_domains.len()
            && client_pane_domains
                .iter()
                .all(|pane_domain| *pane_domain == domain_id)
    })
}

#[cfg(test)]
mod window_close_tests {
    use super::preserve_mux_window_on_gui_close;

    #[test]
    fn close_disposition_uses_origin_then_matching_space_domain() {
        assert!(preserve_mux_window_on_gui_close(Some(7), None, &[7], &[7]));
        assert!(preserve_mux_window_on_gui_close(
            None,
            Some(7),
            &[7, 7],
            &[7, 7]
        ));

        // A tag alone is not a remote session. This is the initial failed
        // connection case: close must remove the mux window and cancel retry.
        assert!(!preserve_mux_window_on_gui_close(Some(7), None, &[], &[]));
        assert!(!preserve_mux_window_on_gui_close(None, None, &[7], &[7]));
        assert!(!preserve_mux_window_on_gui_close(None, Some(7), &[], &[]));
        assert!(!preserve_mux_window_on_gui_close(
            None,
            Some(7),
            &[7, 8],
            &[7]
        ));
        assert!(!preserve_mux_window_on_gui_close(None, Some(7), &[8], &[8]));
    }
}

fn gpu_debug_enabled() -> bool {
    std::env::var_os("THINKTERM_GPU_DEBUG").is_some()
}

fn gpu_debug(message: impl AsRef<str>) {
    if gpu_debug_enabled() {
        log::info!("[gpu-resource] {}", message.as_ref());
    }
}

pub mod background;
pub mod box_model;
pub mod charselect;
pub mod clipboard;
pub mod content_view;
pub mod keyevent;
pub(crate) mod live_overview;
pub mod modal;
mod mouseevent;
pub mod onboarding;
pub mod palette;
pub mod paneselect;
mod prevcursor;
pub(crate) mod project_root_view;
pub(crate) mod remote_files;
pub mod remote_thread_view;
pub(crate) mod remote_walk;
pub mod render;
pub mod resize;
mod selection;
mod sidebar_hover;
mod space_swipe;
pub mod spawn;
pub mod ssh_hosts_view;
pub(crate) mod transfer_walk;
pub mod ui;
pub mod webgpu;

pub(crate) fn theme_aligned_tab_bar_colors_from_palette(palette: &ColorPalette) -> TabBarColors {
    fn rgba(color: LinearRgba) -> RgbaColor {
        color.to_srgb().into()
    }

    let background = palette.resolve_bg(ColorAttribute::Default).to_linear();
    let foreground = palette.foreground.to_linear();
    let muted = foreground.mul_alpha(0.68);

    TabBarColors {
        background: Some(rgba(background)),
        active_tab: Some(TabBarColor {
            bg_color: rgba(background),
            fg_color: palette.foreground.into(),
            ..TabBarColor::default()
        }),
        inactive_tab: Some(TabBarColor {
            bg_color: rgba(background),
            fg_color: rgba(muted),
            ..TabBarColor::default()
        }),
        inactive_tab_hover: Some(TabBarColor {
            bg_color: rgba(background),
            fg_color: palette.foreground.into(),
            ..TabBarColor::default()
        }),
        new_tab: Some(TabBarColor {
            bg_color: rgba(background),
            fg_color: rgba(muted),
            ..TabBarColor::default()
        }),
        new_tab_hover: Some(TabBarColor {
            bg_color: rgba(background),
            fg_color: palette.foreground.into(),
            ..TabBarColor::default()
        }),
        inactive_tab_edge: Some(rgba(background)),
        inactive_tab_edge_hover: Some(rgba(background)),
    }
}

struct ContentViewTab {
    id: ContentViewId,
    key: Option<String>,
    space_id: Option<String>,
    view: Box<dyn ContentView>,
}

/// An in-flight SSH connection started from a `RemoteThreadView`. The view stays
/// foreground as the "Connecting…" UI while we poll the background SSH domain;
/// on success we adopt the materialized workspace, on failure we kill it.
pub(crate) struct RemoteConnectState {
    pub generation: u64,
    pub thread_id: String,
    pub workspace_name: String,
    pub domain_name: String,
    pub started: std::time::Instant,
    /// Startup-created mux window that should be cleaned up only if the remote
    /// workspace is successfully adopted.
    pub orphan_candidate_window_id: Option<MuxWindowId>,
    /// `(domain_name, host_id)` to run OS detection on once connected.
    pub detect_os: Option<(String, String)>,
}

/// An in-flight Mosh connection started from a `RemoteThreadView`. Mosh does
/// not expose an SSH domain status to poll, so the background bootstrap task
/// uses this token to honor cancellation before adopting the workspace.
pub(crate) struct MoshConnectState {
    pub generation: u64,
    pub workspace_name: String,
    pub canceled: Arc<AtomicBool>,
}

/// Latest local Thread activation started by this window. Detached
/// materialization tasks may finish out of order; only the generation that
/// still matches this complete identity may change the visible workspace.
pub(crate) struct LocalThreadActivationState {
    pub generation: u64,
    pub thread_id: String,
    pub space_id: String,
    pub workspace_name: String,
}

use crate::spawn::SpawnWhere;
use prevcursor::PrevCursorPos;

/// What the GPU-side allocator is actually holding, by name.
///
/// The render-cache counters describe CPU-side memory only, which on a DX12 or
/// Vulkan backend is a minority of what the process has committed: those
/// backends sub-allocate through gpu-allocator, and neither the cache lines nor
/// the process totals say whether growth there is live buffers or retained
/// blocks. This distinguishes them: `reserved - allocated` growing means the
/// allocator is holding fragmented blocks, `allocated` growing means live
/// resources are accumulating, and the names say which call site minted them.
///
/// Returns nothing on backends that do not sub-allocate (Metal, GL), which is
/// also why this cannot be inferred from the totals on one platform and applied
/// to another.
fn gpu_allocator_lines(device: &wgpu::Device, label: &str) -> Vec<String> {
    let Some(report) = device.generate_allocator_report() else {
        return vec![];
    };

    let mib = |bytes: u64| format!("{:.1}MiB", bytes as f64 / (1024.0 * 1024.0));
    let mut lines = vec![format!(
        "{label}: gpu_allocator allocated={} reserved={} overhead={} blocks={} live_allocations={}",
        mib(report.total_allocated_bytes),
        mib(report.total_reserved_bytes),
        mib(report
            .total_reserved_bytes
            .saturating_sub(report.total_allocated_bytes)),
        report.blocks.len(),
        report.allocations.len(),
    )];

    // Group by the label wgpu was given at creation, so the biggest consumers
    // are attributable to a call site rather than being one line per buffer.
    let mut by_name: HashMap<&str, (u64, usize)> = HashMap::new();
    for alloc in &report.allocations {
        let name = if alloc.name.is_empty() {
            "<unnamed>"
        } else {
            alloc.name.as_str()
        };
        let entry = by_name.entry(name).or_insert((0, 0));
        entry.0 += alloc.size;
        entry.1 += 1;
    }
    let mut ranked: Vec<_> = by_name.into_iter().collect();
    ranked.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    for (name, (bytes, count)) in ranked.into_iter().take(8) {
        lines.push(format!(
            "{label}: gpu_allocator   {} x{} {}",
            mib(bytes),
            count,
            name
        ));
    }

    // Staging buffers all share one wgpu label, so their sizes are the only
    // thing that says which write_buffer / write_texture call minted them.
    let mut staging_sizes: HashMap<u64, usize> = HashMap::new();
    for alloc in &report.allocations {
        if alloc.name.contains("Staging") {
            *staging_sizes.entry(alloc.size).or_insert(0) += 1;
        }
    }
    if !staging_sizes.is_empty() {
        let mut sizes: Vec<_> = staging_sizes.into_iter().collect();
        sizes.sort_by(|a, b| (b.0 * b.1 as u64).cmp(&(a.0 * a.1 as u64)));
        let summary: Vec<String> = sizes
            .iter()
            .take(6)
            .map(|(size, count)| format!("{}x{}", size, count))
            .collect();
        lines.push(format!(
            "{label}: gpu_allocator   staging_sizes={}",
            summary.join(",")
        ));
    }
    lines
}

/// Byte budgets for the render caches, per TermWindow.
///
/// These caches were built with an entry-count cap and no byte budget, which
/// meant their memory was `cap x whatever an entry happens to weigh` and never
/// shrank once warm -- so a cap read as resident memory multiplied by the
/// number of windows. Every insert already computes a weight (see
/// `estimate_line_quad_entry_bytes` and friends in `render/mod.rs`) and
/// `LfuCache::enforce_limits` already knows how to honour a budget; all that
/// was missing was passing one in.
///
/// Each budget is `configured cap x a per-entry allowance`, not a flat
/// constant, so that raising a cap is not silently defeated by the budget. The
/// cap is the knob a user turns to trade memory for hit rate; the budget only
/// bounds an entry that turns out larger than expected. A flat constant would
/// quietly clamp `line_quad_cache_size` above ~1400 with nothing in the log to
/// say why.
///
/// The floor on any budget is the *visible* working set: `line_quad_cache`
/// needs an entry for every visible line of every pane on every frame, so a
/// budget below that turns each frame into a full miss and costs more CPU than
/// the memory is worth. Measure with `THINKTERM_PERF=1` and the
/// `*_cache_bytes` counters before tightening any of these.
///
/// Entry sizes vary enormously with content, so the allowances are set for the
/// worst case rather than the typical one. `line_quad` measured ~1.2 KiB/entry
/// on ordinary terminal content but ~23 KiB/entry on synthetic lines packed
/// edge to edge with glyphs, and it scales with column count on top of that.
/// A 32 KiB allowance therefore never binds on real content -- these budgets
/// are a ceiling for the pathological case, not a routine reduction. Do not
/// tighten them toward the typical figure: the floor is the visible working
/// set, and dropping below it costs more CPU than the memory is worth.
///
/// `line_state_cache` deliberately has no budget: its entries are a
/// compile-time constant size, so its existing 1024-entry cap already bounds
/// its bytes and a budget would be redundant.
fn shape_cache_budget(config: &ConfigHandle) -> usize {
    config.shape_cache_size.saturating_mul(2 * 1024)
}

fn line_quad_cache_budget(config: &ConfigHandle) -> usize {
    config.line_quad_cache_size.saturating_mul(32 * 1024)
}

fn line_to_ele_shape_cache_budget(config: &ConfigHandle) -> usize {
    config.line_to_ele_shape_cache_size.saturating_mul(2 * 1024)
}

/// How long a pane's overlay scrollbar stays up after the view last moved,
/// and how much of that is spent fading out.
pub(crate) const OVERLAY_SCROLLBAR_SHOW: Duration = Duration::from_millis(1200);
pub(crate) const OVERLAY_SCROLLBAR_FADE: Duration = Duration::from_millis(250);
/// How long the overlay scrollbar takes to grow to its hovered thickness,
/// and to shrink back.
pub(crate) const OVERLAY_SCROLLBAR_EXPAND: Duration = Duration::from_millis(150);
/// The hovered thumb is this much wider than the resting one.
pub(crate) const OVERLAY_SCROLLBAR_HOVER_GROWTH: f32 = 1.2;

const ATLAS_SIZE: usize = 128;

/// Ceiling on growing the glyph atlas to fit a working set, in texels per side.
///
/// [`ATLAS_SIZE`] is only a seed: startup grows it until the utility sprites
/// fit and then stops, which lands at 512 and has nothing to do with how many
/// glyphs are actually in use. The overview pushed the working set well past
/// that -- a terminal's glyphs, the window chrome, and every card's thumbnail
/// at its own font size -- and the overflow path answered by clearing the
/// atlas at the same size. Any single frame fits after a clear, so it never
/// grew and cleared again on the next transition instead: every glyph on
/// screen re-rasterised, several times a minute.
///
/// The atlas is BGRA32, so this is 16MB. Growth stays demand-driven -- nothing
/// is allocated until a frame actually overflows -- and at the ceiling the old
/// clear-in-place behaviour takes over again, which is also what reclaims the
/// one-off glyphs a closed overview leaves behind.
const MAX_GROWN_ATLAS_SIZE: usize = 2048;

/// Absolute ceiling for the glyph atlas on any paint pass, in texels per
/// side; 8192² of RGBA is 256MiB. [`MAX_GROWN_ATLAS_SIZE`] only bounds the
/// first pass: the retry passes grow to whatever the frame asked for, and
/// without this cap that reaches the GPU maximum (16384² = 1GiB) before
/// image downscaling is even attempted. A request past the cap is handled
/// as an allocation failure -- the atlas is cleared in place and
/// `AllowImage` advances -- never by silently rounding the request down,
/// which would rebuild at the same size, succeed, and overflow again on the
/// next pass forever. (Startup lands at 512; 2048 is a conservative floor
/// for any future shrink, not the initial state.)
pub(crate) const MAX_ATLAS_SIZE: usize = 8192;

/// How many atlas-driven repaints one frame may take before it gives up and
/// keeps the previous frame on screen. The worst legitimate case is four
/// doublings up to the cap, one clear, and four image downscale steps.
pub(crate) const MAX_ATLAS_RETRIES: usize = 10;

/// Once a frame had to downscale images to fit the atlas, later frames
/// start at that level for this long instead of probing full size every
/// frame. When it lapses the downscaled sprites are evicted from the frame
/// cache (it is keyed by hash alone and would otherwise keep serving them),
/// the next frame probes full size, and the hold re-arms if that overflows.
pub(crate) const ATLAS_SCALE_HOLD: Duration = Duration::from_secs(30);

/// How many atlas overflow events the memory report keeps.
const ATLAS_OVERFLOW_LOG_LEN: usize = 32;

/// One atlas overflow, kept so the memory report can say what the window
/// was showing when the atlas had to grow or clear.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AtlasOverflowRecord {
    pub at: Instant,
    pub pass: usize,
    pub have: usize,
    pub want: usize,
    pub action: &'static str,
    pub scene: &'static str,
}

/// How long a full-window view takes to arrive or leave.
///
/// Shorter than the Space swipe settle: that one continues a gesture the hand
/// is still invested in, while this is the cost of a glance. The overview is
/// opened to survey what is running, several times an hour, and every
/// millisecond here is charged to that glance.
const CONTENT_VIEW_FADE: Duration = Duration::from_millis(140);

/// How long the terminal takes to travel into the card it becomes.
///
/// Longer than the fade it happens under: the eye is following something
/// across most of the window, and a distance that large read at fade speed
/// registers as a jump rather than as travel.
const CONTENT_VIEW_TRAVEL: Duration = Duration::from_millis(260);

/// How long the travelling terminal takes to dissolve into the card's own
/// thumbnail of it, once the two are close enough in size to overlap.
///
/// Long enough to be seen, which is a lower bound with real teeth: a timeline
/// does not start its clock until the frame after it is armed, so a dissolve
/// budgeted at 45ms spent one frame arming and left two usable ones. That is a
/// cut with extra steps, and it is what the first version of this shipped as.
///
/// Deliberately outlasts the travel. The last stretch of it therefore runs
/// after the terminal has settled into the card, where the two pictures are
/// aligned exactly and the dissolve costs nothing at all.
const CONTENT_VIEW_LANDING_FADE: Duration = Duration::from_millis(140);

/// How long the window frame takes to clear its own edges. Shorter than the
/// terminal's journey: it only has to get out of the way, and the eye should
/// be on the terminal by the time it is halfway across.
const CONTENT_VIEW_CHROME_TRAVEL: Duration = Duration::from_millis(180);

/// How long the frame waits, on the way back, for the terminal to land first.
/// Arriving together would leave the frame sitting around an empty middle.
const CHROME_RETURN_DELAY: Duration = Duration::from_millis(90);

/// Z-index the travelling terminal is composited into: above the arriving
/// view, because it is shrinking *into* the card and has to be seen crossing
/// the grid that is arriving underneath it.
pub(crate) const CONTENT_VIEW_FLIGHT_ZINDEX: i8 = 9;

/// Above every other chrome surface, including the content-view flight layer.
/// A hover tag explains a button that sits at the edge of a sidebar, so it
/// necessarily overhangs that sidebar and must not be painted underneath it.
pub(crate) const TOOLTIP_ZINDEX: i8 = 10;

/// Above even the tooltip layer: the command palette is a modal surface and
/// nothing else may draw over it.
pub(crate) const COMMAND_PALETTE_ZINDEX: i8 = 11;

/// Z-index a transitioning full-window view is composited into.
///
/// Everything else in the window draws at zero. The quad layers within a
/// z-index are a global order rather than per-surface depth, so a view sharing
/// them with the terminal would have the terminal's text drawn through it.
pub(crate) const CONTENT_VIEW_FADE_ZINDEX: i8 = 8;

lazy_static::lazy_static! {
    static ref WINDOW_CLASS: Mutex<String> = Mutex::new(wezterm_gui_subcommands::DEFAULT_WINDOW_CLASS.to_owned());
    static ref POSITION: Mutex<Option<GuiPosition>> = Mutex::new(None);
}

pub const ICON_DATA: &'static [u8] = include_bytes!("../../../assets/icon/ThinkTerm.png");

pub fn set_window_position(pos: GuiPosition) {
    POSITION.lock().unwrap().replace(pos);
}

pub fn set_window_class(cls: &str) {
    *WINDOW_CLASS.lock().unwrap() = cls.to_owned();
}

pub fn get_window_class() -> String {
    WINDOW_CLASS.lock().unwrap().clone()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MouseCapture {
    UI,
    TerminalPane(PaneId),
}

/// Type used together with Window::notify to do something in the
/// context of the window-specific event loop
pub enum TermWindowNotif {
    /// A font fallback resolve completed for exactly these codepoints. Only
    /// entries whose text contains one of them shaped with a placeholder
    /// and need re-shaping; everything else stays warm. The wholesale
    /// variant above cost the Note pane its entire cache several times per
    /// second while a CJK document's fallbacks trickled in.
    InvalidateShapeCacheForChars(Vec<char>),
    /// Invalidate only the terminal-side shape caches (the ones whose
    /// resolved values embed palette colors). The UI text domains key on
    /// `{font_identity, style, text}` with no color anywhere, so a palette
    /// change must not throw them away: pane OSC palette traffic used to
    /// clear the Note caches every frame and re-shape the whole sidebar.
    InvalidateTerminalShapeCache,
    PerformAssignment {
        pane_id: PaneId,
        assignment: KeyAssignment,
        tx: Option<Sender<anyhow::Result<()>>>,
    },
    SetLeftStatus(String),
    SetRightStatus(String),
    GetDimensions(Sender<(Dimensions, WindowState)>),
    GetTerminalSize(Sender<TerminalSize>),
    GetSelectionForPane {
        pane_id: PaneId,
        tx: Sender<String>,
    },
    GetEffectiveConfig(Sender<ConfigHandle>),
    FinishWindowEvent {
        name: String,
        again: bool,
    },
    GetConfigOverrides(Sender<wezterm_dynamic::Value>),
    SetConfigOverrides(wezterm_dynamic::Value),
    CancelOverlayForPane(PaneId),
    CancelOverlayForTab {
        tab_id: TabId,
        pane_id: Option<PaneId>,
    },
    MuxNotification(MuxNotification),
    EmitStatusUpdate,
    OpenProjectPath(PathBuf),
    Apply(Box<dyn FnOnce(&mut TermWindow) + Send + Sync>),
    SwitchToMuxWindow(MuxWindowId),
    SetInnerSize {
        width: usize,
        height: usize,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum NoteEditorCommand {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    Delete,
    SelectAll,
    ReplaceSpelling {
        revision: u64,
        range: Range<usize>,
        replacement: String,
    },
    IgnoreSpelling {
        word: String,
    },
    LearnSpelling {
        word: String,
    },
    LookUp {
        text: String,
        anchor: Rect,
    },
    ToggleSourceMode,
    Save,
    ChooseVault {
        managed: bool,
    },
    NewNote,
    OpenNote {
        relative_path: String,
    },
    RevealVault,
    ToggleVaultTree,
}

#[derive(Clone, Debug)]
pub(crate) enum ContextMenuApplicationAction {
    Note(NoteEditorCommand),
    /// One entry of the Remote Hosts page's card menu, carrying the host it
    /// was raised on. An owned id rather than a list index: the page's
    /// filtered list can change between building the menu and choosing from
    /// it, and an index would then point at a different machine.
    RemoteHost {
        host_id: String,
        command: crate::termwindow::content_view::RemoteHostCommand,
    },
    SetFrontendAccessMode(codec::FrontendAccessMode),
    /// Notification bell entry: jump to a thread, switching Space first when
    /// it lives elsewhere, and acknowledge its unseen work.
    ActivateWorkspaceThread {
        space_id: String,
        thread_id: String,
    },
    /// Sidebar view-options: show/hide threads with this work status.
    ToggleWorkspaceStatusFilter(crate::workspace_threads::WorkspaceThreadWorkStatus),
    /// Sidebar view-options: transiently reveal archived projects.
    ToggleWorkspaceShowArchived,
    /// Add a thread reference to a Space (local, non-home, not already
    /// holding it — the menu only offers eligible targets).
    AddThreadToCollection {
        collection_space_id: Option<String>,
        thread_id: String,
    },
    /// Drop a thread reference from a Space.
    RemoveThreadFromCollection {
        collection_space_id: String,
        thread_id: String,
    },
    /// Re-home a LOCAL thread into another local Space's project — a true
    /// move, unlike the reference the Add action creates.
    MoveThreadToSpace {
        space_id: String,
        thread_id: String,
    },
    /// Move a thread reference from the Space whose sidebar the menu was
    /// opened in over to another Space's ref list.
    MoveThreadRefToSpace {
        from_space_id: String,
        space_id: String,
        thread_id: String,
    },
    /// Fetch a file from the remote Files panel into the Downloads folder.
    DownloadRemoteFile {
        path: remote_files::RemotePath,
        origin: remote_files::RemoteOperationOrigin,
    },
    /// Fetch a whole remote directory. Carries the click position so a
    /// "really download N items?" prompt can appear where the user is looking
    /// — the walk is asynchronous, so the pointer has moved on by then.
    DownloadRemoteFolder {
        path: remote_files::RemotePath,
        anchor: ::window::Point,
        origin: remote_files::RemoteOperationOrigin,
    },
    /// Delete a remote file or folder, after a confirmation anchored likewise.
    DeleteRemoteEntry {
        path: remote_files::RemotePath,
        anchor: ::window::Point,
        origin: remote_files::RemoteOperationOrigin,
    },
    /// Begin an inline rename of a remote row.
    RenameRemoteEntry {
        path: remote_files::RemotePath,
        origin: remote_files::RemoteOperationOrigin,
    },
    /// Create a fresh `untitled folder` under this remote directory.
    NewRemoteFolder {
        parent: remote_files::RemotePath,
        origin: remote_files::RemoteOperationOrigin,
    },
    /// Run a finished transfer again from its recorded source. Carries the
    /// menu's position so a retried folder download can re-raise its size
    /// confirmation in place.
    RetryRemoteTransfer {
        id: u64,
        anchor: ::window::Point,
    },
    /// Clear a finished transfer row away.
    DismissRemoteTransfer(u64),
    /// The user's answer to whatever [`PendingRemoteConfirm`] is waiting.
    ResolveRemoteConfirm(bool),
    /// Copy the remote preview's selected text, or the whole buffer.
    CopyRemotePreviewSelection,
    CopyRemotePreviewAll,
    /// Answer to "these files already exist" for a queued local copy.
    ResolveLocalCopyConflict(transfer_walk::ConflictChoice),
}

/// The scrollbar track a thumb, or the space above or below it, belongs
/// to: which pane it scrolls and where the track runs, so a drag can be
/// turned back into a row without knowing which window layout drew it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollTrack {
    pub pane_id: PaneId,
    /// Top of the track in window pixels, and its height: the range the
    /// thumb's top moves through is `track_top .. track_top + track_height - thumb`.
    pub track_top: usize,
    pub track_height: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UIItemType {
    TabBar(TabBarItem),
    CloseTab(usize),
    PaneNav {
        pane_id: PaneId,
        pane_index: usize,
        action: PaneNavAction,
    },
    ProjectNew,
    SpaceMenu,
    /// "Reconnect" row under the Space menu, shown while the Space's mux
    /// domain is disconnected (retry loop parked, detached, or never
    /// connected this session).
    SpaceReconnect,
    ProjectToggleThreads(String),
    /// Collapse/expand a folder-style thread-reference group. `key` is the
    /// collapse-state key (`"<space_id>::"` + the group key — the origin
    /// project id, or a machine fallback for dangling groups), tracked in
    /// runtime state rather than the store. `origin` is the group's
    /// provenance — origin Space for a local group, machine (and Space) for
    /// a remote one, empty for a dangling group whose header already names
    /// the machine; the row has no room for it, so it is the hover tag.
    ThreadRefGroupToggle {
        key: String,
        origin: String,
    },
    /// The "+" on a reference group: create a thread in the group's origin
    /// project (exactly like the project "+") and auto-add a ref to it in
    /// the current Space. Payload is the group key (origin project id).
    ThreadRefGroupNewThread(String),
    Project(String),
    /// A revealed archived project row: inert except for its context menu,
    /// so it stays invisible to the drag, rename and reorder paths that
    /// match on `Project`.
    ArchivedProject(String),
    WorkspaceThread(String),
    WorkspaceThreadPin(String),
    WorkspaceThreadDelete(String),
    WorkspaceThreadNew(String),
    WorkspaceSidebarToggle,
    WorkspaceSidebarScrollTrack,
    WorkspaceSidebarScrollThumb,
    WorkspaceSidebarHeaderBlank,
    WorkspaceSidebarBackground,
    WorkspaceSidebarResize,
    WorkspaceSidebarSettings,
    WorkspaceSidebarThreadSearch,
    WorkspaceSidebarViewOptions,
    WorkspaceSidebarSshHosts,
    WorkspaceSidebarLiveOverview,
    WorkspaceSidebarNotifications,
    RightSidebarToggle,
    RightSidebarMode(RightSidebarMode),
    RightSidebarBackground,
    RightSidebarResize,
    RightSidebarFilePreviewResize,
    RightSidebarSnippetNew,
    RightSidebarSnippetBack,
    RightSidebarSnippetSave,
    RightSidebarSnippetSearch,
    RightSidebarSnippetTitle,
    RightSidebarSnippetBody,
    RightSidebarSnippetEdit(String),
    RightSidebarSnippetPaste(String),
    RightSidebarSnippetRun(String),
    RightSidebarSnippetDelete(String),
    RightSidebarSnippetScrollTrack,
    RightSidebarSnippetScrollThumb,
    /// A control in the Agents panel.
    RightSidebarAgent(crate::agent_status::AgentPanelAction),
    RightSidebarNoteMenu,
    RightSidebarNoteChooseVault,
    RightSidebarNoteCreateVault,
    /// Re-attempt a vault that failed to open. The point of the button is the
    /// case where nothing about ThinkTerm changed and something outside it did
    /// -- a permission granted, a volume mounted -- so the fix must not be
    /// "restart the app".
    RightSidebarNoteRetry,
    RightSidebarNoteTreeToggle,
    RightSidebarNoteTreeBack,
    RightSidebarNoteTreeRow(String),
    RightSidebarNoteCodeToggle(usize),
    RightSidebarNoteCodeCopy(usize),
    RightSidebarNoteBody,
    RightSidebarNotePaneToggle,
    RightSidebarNotePaneResize,
    RightSidebarFilePreviewScrollTrack,
    RightSidebarFilePreviewScrollThumb,
    RightSidebarFilePreviewHorizontalScrollTrack,
    RightSidebarFilePreviewHorizontalScrollThumb,
    RightSidebarFilePreviewText,
    RightSidebarFileFilter,
    RightSidebarFileRefresh,
    RightSidebarFileRow(PathBuf),
    RightSidebarFileBack,
    RightSidebarFileOpen,
    RightSidebarFileOpenMenu,
    RightSidebarFileReveal,
    RightSidebarFileCopyText,
    RightSidebarRemoteFileConnect,
    RightSidebarRemoteFileRefresh,
    RightSidebarRemoteFileRow(remote_files::RemotePath),
    RightSidebarRemoteFileBack,
    RightSidebarRemoteFileCopyText,
    /// A row in the transfer strip. Clicking cancels it while it runs and
    /// dismisses it once it has finished.
    RightSidebarRemoteTransfer(u64),
    ContextMenuBackdrop,
    ContextMenuItem(Vec<usize>),
    /// Backdrop of the command palette; the palette routes its own pointer
    /// events, this item only keeps clicks from reading as terminal surface.
    CommandPalette,
    AboveScrollThumb(ScrollTrack),
    ScrollThumb(ScrollTrack),
    BelowScrollThumb(ScrollTrack),
    Split(PositionedSplit),
    /// Close button on the synthetic content-view tab.
    ContentViewClose(ContentViewId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightSidebarMode {
    Chat,
    Tasks,
    Snippets,
    /// Agent status panel; only offered while the agent-panel feature
    /// toggle is on (`crate::agent_status::enabled`).
    Agents,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightSidebarNoteView {
    Tree,
    Editor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RightSidebarSnippetView {
    List,
    EditNew,
    EditExisting(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightSidebarSnippetField {
    Search,
    Title,
    Body,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RightSidebarFileView {
    Tree,
    Preview,
}

/// Remembered Files-panel view state for one (root, project), so switching
/// workspaces / idle-release / re-scan can restore where the user was. Holds
/// only paths + scroll + the effective filter (a few KB) — never the index.
#[derive(Clone, Debug)]
pub(crate) struct RightSidebarFileViewState {
    pub view: RightSidebarFileView,
    pub selected: Option<PathBuf>,
    pub expanded: HashSet<String>,
    pub tree_scroll: f32,
    pub preview_scroll: f32,
    pub preview_horizontal: usize,
    pub filter: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightSidebarFileField {
    Filter,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RightSidebarFileTreeRow {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub is_expanded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RightSidebarFileCharBag(pub u128);

#[derive(Clone, Debug)]
pub(crate) struct RightSidebarFileIndexEntry {
    pub path: PathBuf,
    pub name: String,
    pub display_path: String,
    pub is_dir: bool,
    pub name_char_bag: RightSidebarFileCharBag,
    pub char_bag: RightSidebarFileCharBag,
}

/// Flat list of every file in the project, built only to serve fuzzy search.
///
/// Browsing is served by [`RightSidebarFileDirCache`] instead, so this carries
/// no parent/child structure and no per-entry depth: search scans `entries`
/// linearly and ranks the matches itself.
#[derive(Clone, Debug)]
pub(crate) struct RightSidebarFileIndex {
    pub entries: Vec<RightSidebarFileIndexEntry>,
}

/// One child of a directory we actually read. Deliberately smaller than
/// [`RightSidebarFileIndexEntry`]: the browse tree never needs the char bags or
/// display paths that only fuzzy search consumes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RightSidebarFileDirEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

/// Directories the Files tree has actually read, filled on demand as folders are
/// expanded.
///
/// This replaces the old "walk the entire project up front" index for the browse
/// tree. Only the root plus expanded folders are ever read, so opening the panel
/// costs one `read_dir` instead of a full-tree walk, and a project with a huge
/// vendored subtree can no longer starve its own siblings out of the tree.
///
/// `generation` bumps whenever a directory lands or is dropped, and feeds the
/// browse-row cache key so rows rebuild exactly when the contents change.
#[derive(Clone, Debug, Default)]
pub(crate) struct RightSidebarFileDirCache {
    pub dirs: HashMap<PathBuf, Vec<RightSidebarFileDirEntry>>,
    /// Why a directory in `dirs` is empty, when it is empty because the system
    /// refused it rather than because it holds nothing.
    ///
    /// Parallel to `dirs` rather than replacing its value with a `Result` so
    /// that `children()` keeps its signature and every existing caller and test
    /// is untouched. A refused directory is still inserted into `dirs` as an
    /// empty vector: that is what stops the row builder from queueing another
    /// read of it on every rebuild, exactly as it did before failures were
    /// recorded at all.
    pub failures: HashMap<PathBuf, ui::folder_problem::FolderProblem>,
    pub generation: u64,
}

impl RightSidebarFileDirCache {
    pub fn children(&self, dir: &Path) -> Option<&[RightSidebarFileDirEntry]> {
        self.dirs.get(dir).map(|children| children.as_slice())
    }

    /// Why this directory could not be listed, if it could not be.
    pub fn failure(&self, dir: &Path) -> Option<ui::folder_problem::FolderProblem> {
        self.failures.get(dir).copied()
    }

    pub fn is_loaded(&self, dir: &Path) -> bool {
        self.dirs.contains_key(dir)
    }

    /// Record a successful listing. Clearing any previous failure here is what
    /// makes the panel self-heal: the user grants the permission, the re-read
    /// succeeds, and the error card goes away without a restart.
    pub fn insert(&mut self, dir: PathBuf, children: Vec<RightSidebarFileDirEntry>) {
        self.failures.remove(&dir);
        self.dirs.insert(dir, children);
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn insert_failure(&mut self, dir: PathBuf, problem: ui::folder_problem::FolderProblem) {
        self.dirs.insert(dir.clone(), Vec::new());
        self.failures.insert(dir, problem);
        self.generation = self.generation.wrapping_add(1);
    }

    /// Every directory currently held, for the re-scan path.
    pub fn loaded_dirs(&self) -> Vec<PathBuf> {
        self.dirs.keys().cloned().collect()
    }

    pub fn clear(&mut self) {
        self.dirs.clear();
        self.failures.clear();
        self.generation = self.generation.wrapping_add(1);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RightSidebarFileIndexStatus {
    Empty,
    Indexing,
    Ready,
    Failed(String),
}

#[derive(Clone)]
pub(crate) struct RightSidebarFilePreviewSpan {
    pub text: String,
    pub char_count: usize,
    pub color: LinearRgba,
}

#[derive(Clone)]
pub(crate) struct RightSidebarFilePreviewLine {
    pub plain: String,
    pub char_count: usize,
    pub spans: Vec<RightSidebarFilePreviewSpan>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RightSidebarFilePreviewSliceCacheKey {
    pub generation: u64,
    pub line_index: usize,
    pub horizontal_offset: usize,
    pub paint_columns: usize,
    pub highlighted: bool,
}

#[derive(Clone)]
pub(crate) struct RightSidebarFilePreviewSliceCacheValue {
    pub text: String,
    pub colors: Vec<LinearRgba>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UiShapeCacheLookup {
    Hit,
    Miss,
    Skipped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RightSidebarFilePreviewSelectionPoint {
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RightSidebarFilePreviewSelection {
    pub anchor: RightSidebarFilePreviewSelectionPoint,
    pub focus: RightSidebarFilePreviewSelectionPoint,
}

#[derive(Clone)]
pub(crate) struct RightSidebarFilePreviewImage {
    pub data: Arc<ImageData>,
    pub width: u32,
    pub height: u32,
    pub encoded_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RightSidebarNoteImageSource {
    Local(PathBuf),
    Remote(String),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RightSidebarNoteTableLayout {
    pub source_start: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub max_horizontal_scroll: f32,
}

#[derive(Clone, Debug)]
pub(crate) enum RightSidebarOpenWithCacheEntry {
    Loading(u64),
    Ready(Vec<wezterm_open_url::OpenWithCandidate>),
    Failed,
}

/// Painted geometry of a single-line sidebar text input, recorded each frame so
/// a later mouse click can be hit-tested back to a caret position.
#[derive(Clone)]
pub(crate) struct RightSidebarInputLayout {
    pub item_type: UIItemType,
    pub text_x: f32,
    pub text_width: f32,
    pub first_char: usize,
    pub font: Rc<wezterm_font::LoadedFont>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneNavAction {
    Background,
    Activate(PaneId),
    Close(PaneId),
    NewTab,
    ToggleZoom,
    ToggleCollapse,
    SplitRight,
    SplitDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabWheelSurface {
    Window,
    PaneStack(PaneStackId),
}

#[derive(Clone, Debug)]
struct FileDragState {
    payload: FileDragPayload,
    start: ::window::Point,
    current: ::window::Point,
    active: bool,
}

/// A left-sidebar Project or thread row being dragged to a new position in
/// its list. Armed on press; the pending click fires on release when the
/// drag never crossed the movement threshold.
pub(crate) struct SidebarRowDragState {
    pub kind: SidebarRowKind,
    /// The row's display name, for the floating ghost under the cursor.
    pub title: String,
    pub start: ::window::Point,
    pub current: ::window::Point,
    pub active: bool,
    /// Pinned thread rows arm (their click still fires on release) but
    /// never activate: they live in the sidebar's cross-project pinned
    /// section, whose order is derived, not directly editable.
    pub draggable: bool,
    /// A repeating autoscroll tick is already queued; keeps the timer chain
    /// single while the pointer parks in an edge hot zone.
    pub autoscroll_scheduled: bool,
    pub target: Option<SidebarInsertTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SidebarRowKind {
    Project(String),
    /// A thread-ref group header, by its RAW group key (no space prefix);
    /// reorders in the Space's merged folder order alongside projects.
    RefGroup(String),
    Thread {
        thread_id: String,
        project_id: String,
    },
}

/// Where releasing the drag would insert the row: directly before `before`,
/// or last in the list when None. `line_y` is where the insert indicator
/// paints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SidebarInsertTarget {
    pub before: Option<String>,
    pub line_y: isize,
}

/// What a Files-panel drag will paste once it lands on the terminal.
#[derive(Clone, Debug)]
enum FileDragPayload {
    Local(PathBuf),
    /// A remote path only means something together with the source it was
    /// picked from — `/tmp/a` on host A is not the object `/tmp/a` names on
    /// host B — so the drop revalidates that source before pasting.
    Remote {
        path: remote_files::RemotePath,
        origin: remote_files::RemoteOperationOrigin,
    },
}

impl FileDragPayload {
    /// What the floating pill under the cursor says: the file's own name,
    /// falling back to the whole path when there is no name to show.
    fn label(&self) -> String {
        match self {
            Self::Local(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned()),
            Self::Remote { path, .. } => {
                let name = path.file_name();
                if name.is_empty() {
                    path.as_str().to_string()
                } else {
                    name.to_string()
                }
            }
        }
    }
}

/// A planned local copy held back until the user says what to do about files
/// that already exist at the destination.
pub(crate) struct PendingLocalCopy {
    pub plans: Vec<(PathBuf, transfer_walk::TransferPlan)>,
    pub directory: PathBuf,
    /// Destination-relative paths that already exist.
    pub conflicts: std::collections::HashSet<PathBuf>,
    /// What the walk left out, carried through so the finished row can say so
    /// rather than reporting a clean success over silently omitted data.
    pub note: Option<String>,
}

/// A remote operation waiting on an explicit yes from the user before it is
/// allowed to touch anything. One slot, not one per operation: only one
/// confirmation menu can be on screen, and starting a new one supersedes (and
/// cancels) whatever was still waiting.
pub(crate) enum PendingRemoteConfirm {
    /// A folder download whose walk finished large enough to ask about.
    FolderDownload {
        transfer_id: u64,
        plan: remote_walk::RemoteWalkPlan,
        origin: remote_files::RemoteOperationOrigin,
    },
    /// A folder delete: always confirmed, whatever its size — there is no
    /// trash on the far side to undo it from.
    FolderDelete {
        transfer_id: u64,
        remote: remote_files::RemotePath,
        plan: remote_walk::RemoteWalkPlan,
        origin: remote_files::RemoteOperationOrigin,
    },
    /// A single-file delete. No transfer row: it either happens or reports
    /// through the panel's notice line.
    FileDelete {
        remote: remote_files::RemotePath,
        origin: remote_files::RemoteOperationOrigin,
    },
    /// A folder upload whose plan crossed the confirmation threshold.
    FolderUpload {
        transfer_id: u64,
        directory: remote_files::RemotePath,
        plan: transfer_walk::TransferPlan,
        origin: remote_files::RemoteOperationOrigin,
        /// Terminal drops paste the landed folder's remote path here once the
        /// upload finishes; panel drops carry `None`.
        paste_target: Option<(mux::pane::PaneId, remote_files::RemotePath)>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneDropZone {
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneDropKind {
    MoveIntoStack,
    Split,
}

#[derive(Clone, Debug)]
pub(crate) struct PaneTabDropTarget {
    pub target_pane_id: PaneId,
    pub zone: PaneDropZone,
    pub kind: PaneDropKind,
    /// Preview rectangle in window pixels; painted as the drop overlay.
    pub rect: RectF,
}

/// A level-2 pane tab being dragged toward another pane to move or split.
#[derive(Clone, Debug)]
pub(crate) struct PaneTabDragState {
    pub pane_id: PaneId,
    pub title: String,
    pub start: ::window::Point,
    pub current: ::window::Point,
    pub active: bool,
    pub target: Option<PaneTabDropTarget>,
}

/// Split the target pane into a hit grid: an inner box maps to "move into
/// this pane's stack", the border bands map to the nearest edge.
pub(crate) fn pane_drop_zone(fx: f32, fy: f32) -> PaneDropZone {
    if fx > 0.28 && fx < 0.72 && fy > 0.28 && fy < 0.72 {
        return PaneDropZone::Center;
    }
    let mut best = (fx, PaneDropZone::Left);
    for cand in [
        (1.0 - fx, PaneDropZone::Right),
        (fy, PaneDropZone::Top),
        (1.0 - fy, PaneDropZone::Bottom),
    ] {
        if cand.0 < best.0 {
            best = cand;
        }
    }
    best.1
}

pub(crate) fn zone_split_request(zone: PaneDropZone) -> SplitRequest {
    SplitRequest {
        direction: match zone {
            PaneDropZone::Left | PaneDropZone::Right => SplitDirection::Horizontal,
            PaneDropZone::Top | PaneDropZone::Bottom => SplitDirection::Vertical,
            PaneDropZone::Center => unreachable!("center zone does not split"),
        },
        target_is_second: matches!(zone, PaneDropZone::Right | PaneDropZone::Bottom),
        top_level: false,
        size: MuxSplitSize::Percent(50),
    }
}

/// Decide what dropping onto `zone` would do, or None when the drop is a
/// no-op (moving a pane onto the stack it is already in, or splitting a
/// single-pane stack against itself).
pub(crate) fn pane_drop_action(
    zone: PaneDropZone,
    src_in_target_stack: bool,
    target_stack_len: usize,
) -> Option<PaneDropKind> {
    match zone {
        PaneDropZone::Center => {
            if src_in_target_stack {
                None
            } else {
                Some(PaneDropKind::MoveIntoStack)
            }
        }
        _ => {
            if src_in_target_stack && target_stack_len <= 1 {
                None
            } else {
                Some(PaneDropKind::Split)
            }
        }
    }
}

#[cfg(test)]
mod tooltip_tests {
    use super::*;

    #[test]
    fn every_icon_only_sidebar_button_has_a_name() {
        // These five carry no text of their own, so the hover tag is the only
        // place their name appears.
        for item_type in [
            UIItemType::WorkspaceSidebarSettings,
            UIItemType::WorkspaceSidebarThreadSearch,
            UIItemType::WorkspaceSidebarViewOptions,
            UIItemType::WorkspaceSidebarSshHosts,
            UIItemType::WorkspaceSidebarLiveOverview,
            UIItemType::WorkspaceSidebarNotifications,
        ] {
            let label = tooltip_label_for(&item_type)
                .unwrap_or_else(|| panic!("no tooltip label for {:?}", item_type));
            assert!(!label.is_empty(), "empty tooltip label for {:?}", item_type);
            // A missing translation surfaces as the key itself.
            assert!(
                !label.starts_with("tooltip-"),
                "untranslated tooltip label {:?}",
                label
            );
        }
    }

    #[test]
    fn rows_that_already_show_their_own_text_get_no_tag() {
        for item_type in [
            UIItemType::SpaceMenu,
            UIItemType::WorkspaceSidebarBackground,
            UIItemType::Project("p".to_string()),
        ] {
            assert!(tooltip_label_for(&item_type).is_none(), "{:?}", item_type);
        }
    }

    #[test]
    fn a_tag_centres_on_a_button_but_starts_at_a_rows_left_edge() {
        // Icon button: centred on it, regardless of relative widths.
        assert_eq!(tooltip_anchor_x(100.0, 24.0, 80.0, 1000.0, false), 72.0);
        // The nearly-row-wide Settings body still centres — alignment is
        // by item kind, so it cannot flip while the sidebar is resized.
        assert_eq!(tooltip_anchor_x(8.0, 300.0, 80.0, 1000.0, false), 118.0);
        // A list row's tag begins where the row begins — even when the tag
        // is wider than the row (narrow sidebar, long origin), where
        // centring would shove it left of the row.
        assert_eq!(tooltip_anchor_x(8.0, 300.0, 80.0, 1000.0, true), 8.0);
        assert_eq!(tooltip_anchor_x(8.0, 246.0, 300.0, 1000.0, true), 8.0);
        // Neither mode may push the tag off-window.
        assert_eq!(tooltip_anchor_x(0.0, 24.0, 80.0, 1000.0, false), 0.0);
        assert_eq!(tooltip_anchor_x(960.0, 24.0, 80.0, 1000.0, false), 920.0);
        assert_eq!(tooltip_anchor_x(960.0, 40.0, 80.0, 1000.0, true), 920.0);
    }

    /// A reference folder looks like any other folder, so where its threads
    /// actually live is only discoverable through the hover tag: the origin
    /// Space for a local group, machine · Space for a remote one.
    #[test]
    fn a_reference_group_names_its_origin() {
        assert_eq!(
            tooltip_label_for(&UIItemType::ThreadRefGroupToggle {
                key: "project-1".to_string(),
                origin: "Lab Server · Studies".to_string(),
            })
            .as_deref(),
            Some("Lab Server · Studies")
        );
        // A dangling group's header already names the machine; the paint
        // side passes an empty tag and no tooltip may appear.
        assert!(tooltip_label_for(&UIItemType::ThreadRefGroupToggle {
            key: "project-1".to_string(),
            origin: String::new(),
        })
        .is_none());
    }

    /// The Files panel is only ever a sidebar wide, so a long name is cut --
    /// and cut from the end, which is the end that names the file.
    #[test]
    fn a_file_row_names_the_file_in_full() {
        assert_eq!(
            tooltip_label_for(&UIItemType::RightSidebarFileRow(PathBuf::from(
                "/src/a-very-long-component-name.module.tsx"
            )))
            .as_deref(),
            Some("a-very-long-component-name.module.tsx")
        );
        // A search result's row shows a `display_path`, so the ellipsis eats
        // the file name specifically -- the tag has to be what it removed,
        // not the leading directories the row already showed.
        assert_eq!(
            tooltip_label_for(&UIItemType::RightSidebarFileRow(PathBuf::from(
                "/root/wezterm-gui/src/termwindow/ui/right_sidebar.rs"
            )))
            .as_deref(),
            Some("right_sidebar.rs")
        );
        // No final component: nothing to name, and an empty tag must not
        // paint as a bare floating pill.
        assert!(tooltip_label_for(&UIItemType::RightSidebarFileRow(PathBuf::from("/"))).is_none());
        // Remote rows are the same full-width, end-ellipsized row painted
        // from an SFTP listing, and must carry the same tag.
        assert_eq!(
            tooltip_label_for(&UIItemType::RightSidebarRemoteFileRow(
                remote_files::RemotePath::from_server_absolute("/home/x/right_sidebar.rs").unwrap()
            ))
            .as_deref(),
            Some("right_sidebar.rs")
        );
        assert!(tooltip_label_for(&UIItemType::RightSidebarRemoteFileRow(
            remote_files::RemotePath::from_server_absolute("/").unwrap()
        ))
        .is_none());
    }

    /// Alignment is a property of the item's KIND. Kept as a named rule so a
    /// third full-width row cannot quietly inherit the button behaviour.
    #[test]
    fn full_width_rows_left_align_their_tag_and_buttons_do_not() {
        assert!(tooltip_left_aligns(&UIItemType::RightSidebarFileRow(
            PathBuf::from("/a/b.rs")
        )));
        assert!(tooltip_left_aligns(&UIItemType::RightSidebarRemoteFileRow(
            remote_files::RemotePath::from_server_absolute("/a/b.rs").unwrap()
        )));
        assert!(tooltip_left_aligns(&UIItemType::ThreadRefGroupToggle {
            key: "project-1".to_string(),
            origin: "Lab Server".to_string(),
        }));
        assert!(!tooltip_left_aligns(&UIItemType::WorkspaceSidebarSettings));
    }
}

#[cfg(test)]
mod pane_drop_tests {
    use super::*;

    #[test]
    fn drop_zone_center_box_and_edges() {
        assert_eq!(pane_drop_zone(0.5, 0.5), PaneDropZone::Center);
        assert_eq!(pane_drop_zone(0.29, 0.29), PaneDropZone::Center);
        assert_eq!(pane_drop_zone(0.71, 0.71), PaneDropZone::Center);
        // On/outside the inner box: nearest edge wins
        assert_eq!(pane_drop_zone(0.1, 0.5), PaneDropZone::Left);
        assert_eq!(pane_drop_zone(0.9, 0.5), PaneDropZone::Right);
        assert_eq!(pane_drop_zone(0.5, 0.1), PaneDropZone::Top);
        assert_eq!(pane_drop_zone(0.5, 0.9), PaneDropZone::Bottom);
        // Corners resolve to whichever edge is closest
        assert_eq!(pane_drop_zone(0.05, 0.2), PaneDropZone::Left);
        assert_eq!(pane_drop_zone(0.2, 0.05), PaneDropZone::Top);
        assert_eq!(pane_drop_zone(0.98, 0.9), PaneDropZone::Right);
        assert_eq!(pane_drop_zone(0.9, 0.98), PaneDropZone::Bottom);
        // Extremes
        assert_eq!(pane_drop_zone(0.0, 0.5), PaneDropZone::Left);
        assert_eq!(pane_drop_zone(1.0, 0.5), PaneDropZone::Right);
    }

    #[test]
    fn zone_to_split_request_mapping() {
        let left = zone_split_request(PaneDropZone::Left);
        assert_eq!(left.direction, SplitDirection::Horizontal);
        assert!(!left.target_is_second);

        let right = zone_split_request(PaneDropZone::Right);
        assert_eq!(right.direction, SplitDirection::Horizontal);
        assert!(right.target_is_second);

        let top = zone_split_request(PaneDropZone::Top);
        assert_eq!(top.direction, SplitDirection::Vertical);
        assert!(!top.target_is_second);

        let bottom = zone_split_request(PaneDropZone::Bottom);
        assert_eq!(bottom.direction, SplitDirection::Vertical);
        assert!(bottom.target_is_second);

        for zone in [
            PaneDropZone::Left,
            PaneDropZone::Right,
            PaneDropZone::Top,
            PaneDropZone::Bottom,
        ] {
            let request = zone_split_request(zone);
            assert!(!request.top_level);
            assert_eq!(request.size, MuxSplitSize::Percent(50));
        }
    }

    #[test]
    fn drop_action_table() {
        // Moving into a stack the pane already lives in is a no-op
        assert_eq!(pane_drop_action(PaneDropZone::Center, true, 2), None);
        assert_eq!(
            pane_drop_action(PaneDropZone::Center, false, 1),
            Some(PaneDropKind::MoveIntoStack)
        );
        // Splitting a single-pane stack against itself is a no-op
        assert_eq!(pane_drop_action(PaneDropZone::Left, true, 1), None);
        // ...but a background tab can split out of its own stack
        assert_eq!(
            pane_drop_action(PaneDropZone::Bottom, true, 2),
            Some(PaneDropKind::Split)
        );
        for zone in [
            PaneDropZone::Left,
            PaneDropZone::Right,
            PaneDropZone::Top,
            PaneDropZone::Bottom,
        ] {
            assert_eq!(pane_drop_action(zone, false, 1), Some(PaneDropKind::Split));
        }
    }
}

#[derive(Clone, Debug)]
enum InlineTabRenameTarget {
    WindowTab(TabId),
    PaneTab(PaneId),
    Space(String),
    Project(String),
    WorkspaceThread(String),
    /// A file row in the right sidebar Files panel; commits via fs::rename.
    File(PathBuf),
    /// A row in the REMOTE Files panel; commits via an SFTP rename, so unlike
    /// every other target the commit is asynchronous and reports its failure
    /// through the panel's notice line rather than a log.
    RemoteFile {
        path: remote_files::RemotePath,
        source_key: String,
    },
}

#[derive(Clone, Debug)]
struct InlineTabRename {
    target: InlineTabRenameTarget,
    input: TextInputState,
}

impl InlineTabRename {
    fn new(target: InlineTabRenameTarget, text: String) -> Self {
        let mut input = TextInputState::new();
        input.set_text_end(text);
        input.caret_select_all();
        Self { target, input }
    }

    /// Legacy tab surfaces still paint their inline editor as title text. File
    /// rows use the real shared text-input painter; keep this adapter for tabs
    /// until those surfaces move to the same widget.
    fn display_text(&self) -> String {
        if self.input.caret_selection_range().is_some() {
            return if self.input.is_empty() {
                "|".to_string()
            } else {
                self.input.text().to_string()
            };
        }

        let mut text = String::new();
        let mut inserted_cursor = false;
        for (idx, ch) in self.input.text().chars().enumerate() {
            if idx == self.input.cursor {
                text.push('|');
                inserted_cursor = true;
            }
            text.push(ch);
        }
        if !inserted_cursor {
            text.push('|');
        }
        text
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UIItem {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    pub item_type: UIItemType,
}

/// How long the pointer must rest on an icon-only button before its name
/// appears. Long enough that crossing the sidebar on the way somewhere else
/// shows nothing.
pub const TOOLTIP_DELAY: Duration = Duration::from_millis(500);

/// A pending or visible hover tag: which button, where it sits, and when the
/// pointer arrived.
#[derive(Clone, Debug)]
pub struct HoverTooltip {
    pub item: UIItem,
    pub since: Instant,
}

/// Where a hover tag's left edge goes, given the rect it names.
///
/// Alignment is decided by what KIND of item is tagged, never inferred from
/// widths: a width heuristic silently re-anchors whichever control happens
/// to cross the tag's width — the Settings row is nearly sidebar-wide while
/// its label shows, then collapses to an icon pill, and its tag must not
/// flip alignment mid-resize. Buttons centre (the tag reads as belonging to
/// the icon); full-width list rows left-align to the row's start (they have
/// no centre worth pointing at). Kept clear of the window edges either way.
pub fn tooltip_anchor_x(
    item_x: f32,
    item_width: f32,
    tip_width: f32,
    window_width: f32,
    left_align: bool,
) -> f32 {
    let x = if left_align {
        item_x
    } else {
        item_x + (item_width - tip_width) / 2.0
    };
    x.clamp(0.0, (window_width - tip_width).max(0.0))
}

/// Whether a tag begins at its item's left edge instead of centring on it.
///
/// By item KIND, never by width -- see [`tooltip_anchor_x`] for why. Full-width
/// list rows have no centre worth pointing at; icon buttons do.
pub fn tooltip_left_aligns(item_type: &UIItemType) -> bool {
    matches!(
        item_type,
        UIItemType::ThreadRefGroupToggle { .. }
            | UIItemType::RightSidebarFileRow(_)
            | UIItemType::RightSidebarRemoteFileRow(_)
    )
}

/// The tag to show for an item whose own text cannot say enough: an icon-only
/// button (no text at all) or a row whose label is cut to fit the sidebar.
/// `None` for everything that already reads in full.
pub fn tooltip_label_for(item_type: &UIItemType) -> Option<String> {
    // A reference group's header shows the origin project's name, exactly
    // like a local folder. Where that project actually lives — which Space,
    // which machine — has nowhere to go on a sidebar-width row, so the
    // hover tag is where it appears.
    if let UIItemType::ThreadRefGroupToggle { origin, .. } = item_type {
        return (!origin.is_empty()).then(|| origin.clone());
    }
    // A file row gets only the sidebar's width, and `ellipsize_ui_text` cuts
    // from the END -- so what survives is the part every sibling shares and
    // what is lost is the part that identifies the file. The tag carries the
    // file name in both modes: in the tree it is what the row already tried
    // to show, and in search results (where the row shows a `display_path`)
    // it is exactly the tail the ellipsis ate.
    if let UIItemType::RightSidebarFileRow(path) = item_type {
        return path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty());
    }
    // Remote file rows are the same full-width, end-ellipsized row painted
    // from an SFTP listing; the tag recovers the same tail.
    if let UIItemType::RightSidebarRemoteFileRow(path) = item_type {
        let name = path.file_name();
        return (!name.is_empty()).then(|| name.to_string());
    }
    let key = match item_type {
        UIItemType::WorkspaceSidebarSettings => "tooltip-sidebar-settings",
        UIItemType::WorkspaceSidebarThreadSearch => "tooltip-sidebar-thread-search",
        UIItemType::WorkspaceSidebarViewOptions => "tooltip-sidebar-view-options",
        UIItemType::WorkspaceSidebarSshHosts => "tooltip-sidebar-ssh-hosts",
        UIItemType::WorkspaceSidebarLiveOverview => "tooltip-sidebar-live-overview",
        UIItemType::WorkspaceSidebarNotifications => "tooltip-sidebar-notifications",
        _ => return None,
    };
    Some(crate::i18n::tr(key))
}

impl UIItem {
    pub fn hit_test(&self, x: isize, y: isize) -> bool {
        x >= self.x as isize
            && x <= (self.x + self.width) as isize
            && y >= self.y as isize
            && y <= (self.y + self.height) as isize
    }
}

#[derive(Clone, Default)]
pub struct SemanticZoneCache {
    seqno: SequenceNo,
    zones: Vec<StableRowIndex>,
}

pub struct OverlayState {
    pub pane: Arc<dyn Pane>,
    pub key_table_state: KeyTableState,
}

#[derive(Default)]
pub struct PaneState {
    /// If is_some(), the top row of the visible screen.
    /// Otherwise, the viewport is at the bottom of the
    /// scrollback.
    viewport: Option<StableRowIndex>,
    /// Smooth scrolling: how far, in physical pixels, the row `viewport`
    /// is cut off at its top. Always in `[0, cell height)` and always 0
    /// while following the bottom; every row-granular caller of
    /// `set_viewport` leaves it 0.
    viewport_px: f32,
    /// Smooth scrolling: distance, in physical pixels, that a wheel notch
    /// still has to travel. Consumed a fraction per frame by
    /// `advance_scroll_glide`; positive is down, towards newer rows.
    glide_remaining: f32,
    /// When the glide last advanced, so a frame that comes late moves the
    /// viewport by the time that actually passed.
    glide_last_tick: Option<Instant>,
    /// Overlay scrollbar: until when the pane's indicator stays up. Set
    /// whenever the viewport moves, or the thumb is hovered or dragged;
    /// the last stretch before it is spent fading out.
    scrollbar_visible_until: Option<Instant>,
    /// Overlay scrollbar: how far the thumb has grown towards its hovered
    /// thickness, 0 (resting) to 1 (pointer on it or dragging it), moved
    /// a little each frame by `paint_overlay_scrollbar`.
    scrollbar_expand: f32,
    /// When `scrollbar_expand` last moved, so the growth is paced by wall
    /// time rather than by however many frames happened to paint.
    scrollbar_expand_tick: Option<Instant>,
    selection: Selection,
    /// If is_some(), rather than display the actual tab
    /// contents, we're overlaying a little internal application
    /// tab.  We'll also route input to it.
    pub overlay: Option<OverlayState>,

    bell_start: Option<Instant>,
    pub mouse_terminal_coords: Option<(ClickPosition, StableRowIndex)>,
    pub font_scale: Option<f64>,
    /// Latest durable Mux PaneOutput generation included in a frame this
    /// native window successfully presented.
    presented_output_generation: u64,
}

fn pane_output_needs_repaint(current: u64, presented: u64) -> bool {
    current != presented
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PaintOutcome {
    draw_submitted: bool,
    frame_complete: bool,
}

fn frame_can_acknowledge_output(outcome: PaintOutcome, presented: bool) -> bool {
    presented && outcome.draw_submitted && outcome.frame_complete
}

#[cfg(test)]
mod stale_viewport_tests {
    use super::TermWindow;
    use mux::renderable::RenderableDimensions;

    fn dims(scrollback_top: isize, physical_top: isize) -> RenderableDimensions {
        RenderableDimensions {
            scrollback_top,
            physical_top,
            ..Default::default()
        }
    }

    #[test]
    fn valid_viewports_pass_through_untouched() {
        assert_eq!(
            TermWindow::normalize_stale_viewport(None, &dims(100, 500)),
            None
        );
        assert_eq!(
            TermWindow::normalize_stale_viewport(Some(100), &dims(100, 500)),
            None
        );
        assert_eq!(
            TermWindow::normalize_stale_viewport(Some(499), &dims(100, 500)),
            None
        );
    }

    #[test]
    fn overtaken_viewport_clamps_to_oldest_retained_row() {
        assert_eq!(
            TermWindow::normalize_stale_viewport(Some(40), &dims(100, 500)),
            Some(Some(100))
        );
    }

    #[test]
    fn overtaken_viewport_follows_bottom_when_no_scrollback_remains() {
        // The app erased its scrollback (agent CLIs do this on redraw):
        // scrollback_top == physical_top, so "the oldest retained row" IS
        // the live screen and the pane must convert to follow-bottom
        // rather than stay artificially pinned.
        assert_eq!(
            TermWindow::normalize_stale_viewport(Some(40), &dims(500, 500)),
            Some(None)
        );
    }
}

#[cfg(test)]
mod pane_output_watchdog_tests {
    use super::{frame_can_acknowledge_output, pane_output_needs_repaint, PaintOutcome};

    #[test]
    fn a_later_output_remains_unpresented_after_an_older_frame_commits() {
        let captured = 4;
        let current_after_paint = 5;
        assert!(frame_can_acknowledge_output(
            PaintOutcome {
                draw_submitted: true,
                frame_complete: true,
            },
            true,
        ));
        assert!(pane_output_needs_repaint(current_after_paint, captured));
        assert!(!pane_output_needs_repaint(captured, captured));
    }

    #[test]
    fn incomplete_failed_or_skipped_frames_never_acknowledge_output() {
        assert!(!frame_can_acknowledge_output(
            PaintOutcome {
                draw_submitted: true,
                frame_complete: false,
            },
            true,
        ));
        assert!(!frame_can_acknowledge_output(
            PaintOutcome {
                draw_submitted: false,
                frame_complete: true,
            },
            true,
        ));
        assert!(!frame_can_acknowledge_output(
            PaintOutcome {
                draw_submitted: true,
                frame_complete: true,
            },
            false,
        ));
    }
}

#[derive(Clone)]
pub struct PaneFontEntry {
    pub fonts: Rc<FontConfiguration>,
    pub render_metrics: RenderMetrics,
    /// LRU stamp from `pane_font_cache_tick`; the cache holds whole
    /// FontConfigurations and would otherwise only ever grow.
    last_used: Cell<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneFontKey {
    font_scale_bits: u64,
    dpi: usize,
    config_generation: usize,
}

/// Data used when synchronously formatting pane and window titles
#[derive(Debug, Clone)]
pub struct TabInformation {
    pub tab_id: TabId,
    pub tab_index: usize,
    pub is_active: bool,
    pub is_last_active: bool,
    pub active_pane: Option<PaneInformation>,
    pub window_id: MuxWindowId,
    pub tab_title: String,
}

impl UserData for TabInformation {
    fn add_fields<'lua, F: UserDataFields<'lua, Self>>(fields: &mut F) {
        fields.add_field_method_get("tab_id", |_, this| Ok(this.tab_id));
        fields.add_field_method_get("tab_index", |_, this| Ok(this.tab_index));
        fields.add_field_method_get("is_active", |_, this| Ok(this.is_active));
        fields.add_field_method_get("is_last_active", |_, this| Ok(this.is_last_active));
        fields.add_field_method_get("active_pane", |_, this| {
            if let Some(pane) = &this.active_pane {
                Ok(Some(pane.clone()))
            } else {
                Ok(None)
            }
        });
        fields.add_field_method_get("panes", |_, this| {
            let mux = Mux::get();
            let mut panes = vec![];
            if let Some(tab) = mux.get_tab(this.tab_id) {
                panes = tab
                    .iter_panes()
                    .iter()
                    .map(TermWindow::pos_pane_to_pane_info)
                    .collect();
            }
            Ok(panes)
        });
        fields.add_field_method_get("window_id", |_, this| Ok(this.window_id));
        fields.add_field_method_get("tab_title", |_, this| Ok(this.tab_title.clone()));
        fields.add_field_method_get("window_title", |_, this| {
            let mux = Mux::get();
            let window = mux.get_window(this.window_id).ok_or_else(|| {
                mlua::Error::external(format!("window {} not found", this.window_id))
            })?;
            Ok(window.get_title().to_string())
        });
    }
}

/// Data used when synchronously formatting pane and window titles
#[derive(Debug, Clone)]
pub struct PaneInformation {
    pub pane_id: PaneId,
    pub pane_index: usize,
    pub is_active: bool,
    pub is_zoomed: bool,
    pub has_unseen_output: bool,
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub title: String,
    pub user_vars: HashMap<String, String>,
    pub progress: Progress,
}

impl UserData for PaneInformation {
    fn add_fields<'lua, F: UserDataFields<'lua, Self>>(fields: &mut F) {
        fields.add_field_method_get("pane_id", |_, this| Ok(this.pane_id));
        fields.add_field_method_get("pane_index", |_, this| Ok(this.pane_index));
        fields.add_field_method_get("is_active", |_, this| Ok(this.is_active));
        fields.add_field_method_get("is_zoomed", |_, this| Ok(this.is_zoomed));
        fields.add_field_method_get("has_unseen_output", |_, this| Ok(this.has_unseen_output));
        fields.add_field_method_get("left", |_, this| Ok(this.left));
        fields.add_field_method_get("top", |_, this| Ok(this.top));
        fields.add_field_method_get("width", |_, this| Ok(this.width));
        fields.add_field_method_get("height", |_, this| Ok(this.height));
        fields.add_field_method_get("pixel_width", |_, this| Ok(this.pixel_width));
        fields.add_field_method_get("pixel_height", |_, this| Ok(this.pixel_height));
        fields.add_field_method_get("progress", |lua, this| lua.to_value(&this.progress));
        fields.add_field_method_get("title", |_, this| Ok(this.title.clone()));
        fields.add_field_method_get("user_vars", |_, this| Ok(this.user_vars.clone()));
        fields.add_field_method_get("foreground_process_name", |_, this| {
            let mut name = None;
            if let Some(mux) = Mux::try_get() {
                if let Some(pane) = mux.get_pane(this.pane_id) {
                    name = pane.get_foreground_process_name(CachePolicy::AllowStale);
                }
            }
            match name {
                Some(name) => Ok(name),
                None => Ok("".to_string()),
            }
        });
        fields.add_field_method_get("tty_name", |_, this| {
            let mut name = None;
            if let Some(mux) = Mux::try_get() {
                if let Some(pane) = mux.get_pane(this.pane_id) {
                    name = pane.tty_name();
                }
            }
            Ok(name)
        });
        fields.add_field_method_get("current_working_dir", |_, this| {
            if let Some(mux) = Mux::try_get() {
                if let Some(pane) = mux.get_pane(this.pane_id) {
                    return Ok(pane
                        .get_current_working_dir(CachePolicy::AllowStale)
                        .map(|url| url_funcs::Url { url }));
                }
            }
            Ok(None)
        });
        fields.add_field_method_get("domain_name", |_, this| {
            let mut name = None;
            if let Some(mux) = Mux::try_get() {
                if let Some(pane) = mux.get_pane(this.pane_id) {
                    let domain_id = pane.domain_id();
                    name = mux
                        .get_domain(domain_id)
                        .map(|dom| dom.domain_name().to_string());
                }
            }
            match name {
                Some(name) => Ok(name),
                None => Ok("".to_string()),
            }
        });
    }
}

#[derive(Default)]
pub struct TabState {
    /// If is_some(), rather than display the actual tab
    /// contents, we're overlaying a little internal application
    /// tab.  We'll also route input to it.
    pub overlay: Option<OverlayState>,
}

/// Manages the state/queue of lua based event handlers.
/// We don't want to queue more than 1 event at a time,
/// so we use this enum to allow for at most 1 executing
/// and 1 pending event.
#[derive(Copy, Clone, Debug)]
enum EventState {
    /// The event is not running
    None,
    /// The event is running
    InProgress,
    /// The event is running, and we have another one ready to
    /// run once it completes
    InProgressWithQueued(Option<PaneId>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrontendGeometryPhase {
    Previewing { epoch: u64 },
    Committing { epoch: u64 },
    TakeoverSyncing { epoch: u64 },
}

impl FrontendGeometryPhase {
    pub(crate) fn epoch(self) -> u64 {
        match self {
            Self::Previewing { epoch }
            | Self::Committing { epoch }
            | Self::TakeoverSyncing { epoch } => epoch,
        }
    }

    pub(crate) fn obscures_terminal(self) -> bool {
        matches!(self, Self::TakeoverSyncing { .. })
    }

    pub(crate) fn is_in_flight(self) -> bool {
        matches!(self, Self::Committing { .. } | Self::TakeoverSyncing { .. })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FrontendGeometryConfirmation {
    pub(crate) epoch: u64,
    pub(crate) panes: Vec<(PaneId, TerminalSize)>,
    pub(crate) ready_since: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FrontendRecoveryGeometry {
    pub(crate) epoch: u64,
    pub(crate) domain_id: DomainId,
    pub(crate) slot: FrontendRecoverySlot,
    pub(crate) generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteDividerResizeStrategy {
    Live,
    OnRelease,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedRemoteDividerGeometry {
    pub(crate) viewport: codec::ClientViewport,
    pub(crate) panes: Vec<(PaneId, TerminalSize)>,
}

#[derive(Debug, Clone)]
pub(crate) struct RemoteDividerResizeStream {
    pub(crate) epoch: u64,
    pub(crate) strategy: RemoteDividerResizeStrategy,
    pub(crate) domain_id: DomainId,
    pub(crate) connection_generation: u64,
    pub(crate) claim_first: bool,
    pub(crate) finishing: bool,
    pub(crate) latest: PreparedRemoteDividerGeometry,
    pub(crate) pending: Option<PreparedRemoteDividerGeometry>,
    pub(crate) in_flight: Option<codec::ClientViewport>,
    pub(crate) last_acknowledged: Option<codec::ClientViewport>,
    pub(crate) final_acknowledged_at: Option<Instant>,
}

/// A recorded left sidebar, kept so the outgoing Space can be composited
/// against the incoming one.
pub(crate) struct CapturedSidebar {
    pub(crate) quads: HeapQuadAllocator,
    /// The stretch of `quads` holding the scrolling project list. The rest is
    /// chrome that deliberately overlaps it -- the bottom fade, the settings
    /// row -- so only a split by paint order, never by screen position, can
    /// separate what slides from what stays.
    pub(crate) list: Option<(HeapQuadMark, HeapQuadMark)>,
}

/// How many times a restored window may be put back on its remembered frame
/// while startup settles. Enough for the handful of resizes a launch produces,
/// short enough that it cannot turn into a loop.
const RESTORED_FRAME_ATTEMPTS: u8 = 8;

/// How long after opening a restored window its frame is still worth
/// correcting. Long enough to cover the resizes a launch produces, short
/// enough that it is over before the user has reached for the window.
const RESTORED_FRAME_SETTLE_TIME: Duration = Duration::from_secs(3);

pub struct TermWindow {
    pub window: Option<Window>,
    pub config: ConfigHandle,
    pub config_overrides: wezterm_dynamic::Value,
    os_parameters: Option<parameters::Parameters>,
    /// When we most recently received keyboard focus
    pub focused: Option<Instant>,
    /// When the window stopped being visible to the user (fully covered,
    /// minimized, on another macOS Space, app hidden). None while
    /// visible. The timestamp doubles as the start of the grace period
    /// before occlusion-driven cache release.
    occluded: Option<Instant>,
    /// Whether the current occlusion episode's cache release already
    /// ran. Reset when the window goes occluded and when a hidden
    /// repaint regrows what the release dropped, so the heartbeat can
    /// release again.
    occlusion_released: bool,
    /// When the last occlusion release ran; damps re-releases to at most
    /// one per grace period even if something keeps repainting the
    /// hidden window (snapshot and capture contexts can draw it).
    occlusion_last_release: Option<Instant>,
    fonts: Rc<FontConfiguration>,
    /// Window dimensions and dpi
    pub dimensions: Dimensions,
    pub window_state: WindowState,
    pub resizes_pending: usize,
    is_repaint_pending: bool,
    pending_scale_changes: LinkedList<resize::ScaleChange>,
    /// Terminal dimensions
    terminal_size: TerminalSize,
    /// Per-tab preview and submission state. Current-owner geometry commits
    /// remain visible; only a real takeover or unknown initial owner obscures
    /// the terminal until the matching epoch is acknowledged.
    frontend_geometry_phases: HashMap<TabId, FrontendGeometryPhase>,
    /// A takeover RPC acknowledges PTY resize before the remote application
    /// redraw has reached this renderer. Keep its expected pane snapshots here
    /// and reveal only after every visible row has been refreshed and stable.
    frontend_geometry_confirmations: HashMap<TabId, FrontendGeometryConfirmation>,
    /// Replacement-mux recovery is completed only after the matching geometry
    /// epoch reaches the normal local render-surface confirmation point.
    frontend_recovery_geometry: HashMap<TabId, FrontendRecoveryGeometry>,
    frontend_geometry_resync_after_epoch: HashSet<TabId>,
    /// The last locally-published viewport the mux rejected, per tab. Blocks
    /// republishing an identical viewport against an identical tab shape,
    /// which would otherwise loop: reject → lease revoked → notification →
    /// republish → reject. See `resize::RejectedLocalViewport`.
    rejected_local_viewports: HashMap<TabId, resize::RejectedLocalViewport>,
    /// Remote counterpart: the last ClientPane viewport the remote mux
    /// rejected, per tab. Same loop, but each retry also costs a resync, and
    /// a wire failure is not always deterministic - so identical retries are
    /// paced rather than silenced. See `resize::RejectedClientViewport`.
    rejected_client_viewports: HashMap<TabId, resize::RejectedClientViewport>,
    /// Monotonic id of the newest remote viewport publish spawned per tab.
    /// Completions compare against it so an out-of-order finish (they are
    /// detached tasks, and failures also await a resync) cannot mutate the
    /// rejection damper with a stale decision.
    client_viewport_publish_seq: HashMap<TabId, u64>,
    /// File rows whose painted label was actually cut this frame, rebuilt by
    /// the Files panel painters each paint. Gates the hover tag: a row that
    /// already reads in full has nothing for a tag to add.
    right_sidebar_truncated_file_rows: HashSet<PathBuf>,
    right_sidebar_truncated_remote_file_rows: HashSet<remote_files::RemotePath>,
    /// Latest-only full-viewport streams for native divider drags. A stream
    /// owns the matching ClientPane preview epoch until its final target is
    /// confirmed or explicitly rolled back.
    remote_divider_resize_streams: HashMap<TabId, RemoteDividerResizeStream>,
    next_frontend_geometry_epoch: u64,
    frontend_viewport_report_pending: Arc<AtomicBool>,
    /// A blocked press is consumed even if the takeover round-trip completes
    /// before its matching release arrives.
    frontend_handoff_consumed_press: bool,
    pub mux_window_id: MuxWindowId,
    pub mux_window_id_for_subscriptions: Arc<Mutex<MuxWindowId>>,
    /// Kill switch for the currently registered mux pane-update
    /// subscription. subscribe_to_pane_updates retires the previous
    /// subscription through this before installing a new one, so a window
    /// can re-subscribe at any time without accumulating duplicates.
    pane_subscription_dead: RefCell<Option<Arc<AtomicBool>>>,
    /// Consecutive render-watchdog ticks that had to force a repaint for
    /// unpresented output. A run of these means output notifications are
    /// not reaching this window at all (a dead subscription), not a lost
    /// frame; the watchdog then rebuilds the subscription — once per
    /// episode, because a pane that legitimately never presents (a hidden
    /// stack member) keeps the watchdog latched forever and must not turn
    /// the heal into a warn-spamming resubscribe loop.
    watchdog_forced_repaints: u8,
    /// Per pane, the presented generation at the last tick that found the
    /// pane behind: painting that still moves between ticks is a busy pane,
    /// not a lost frame.
    watchdog_presented_seen: HashMap<PaneId, u64>,
    /// When the counter above last advanced. The watchdog can run in
    /// sub-second bursts (status updates queue up), so increments are
    /// paced to at most one per ~700ms to approximate "consecutive
    /// seconds behind".
    watchdog_last_forced: Option<Instant>,
    pub render_metrics: RenderMetrics,
    render_state: Option<RenderState>,
    input_map: InputMap,
    /// If is_some, the LEADER modifier is active until the specified instant.
    leader_is_down: Option<std::time::Instant>,
    dead_key_status: DeadKeyStatus,
    key_table_state: KeyTableState,
    show_tab_bar: bool,
    show_scroll_bar: bool,
    tab_bar: TabBarState,
    fancy_tab_bar: Option<box_model::ComputedElement>,
    tab_bar_scroll_offset: f32,
    tab_bar_scroll_target: f32,
    pane_nav_tab_scroll_offsets: HashMap<PaneId, f32>,
    pane_nav_tab_scroll_targets: HashMap<PaneId, f32>,
    collapsed_pane_layouts: HashMap<PaneStackId, CollapsedPaneLayout>,
    tab_wheel_surface_lock: Option<(TabWheelSurface, Instant)>,
    tab_wheel_direction_lock: Option<(TabWheelSurface, i16, Instant)>,
    inline_tab_rename: Option<InlineTabRename>,
    pane_tab_title_overrides: HashMap<PaneId, String>,
    pub right_status: String,
    pub left_status: String,
    last_ui_item: Option<UIItem>,
    /// The icon-only button the pointer is resting on, and when it arrived.
    /// The tag is only painted once it has been there for TOOLTIP_DELAY, so
    /// that sweeping the pointer across the sidebar does not strobe labels.
    hover_tooltip: Option<HoverTooltip>,
    /// Tracks whether the current mouse-down event is part of click-focus.
    /// If so, we ignore mouse events until released
    is_click_to_focus_window: bool,
    last_mouse_coords: (usize, i64),
    window_drag_position: Option<MouseEvent>,
    current_mouse_event: Option<MouseEvent>,
    prev_cursor: PrevCursorPos,
    last_scroll_info: RenderableDimensions,

    tab_state: RefCell<HashMap<TabId, TabState>>,
    pane_state: RefCell<HashMap<PaneId, PaneState>>,
    /// Generations read before the terminal lines used by the current paint
    /// pass. Retried or abandoned passes replace this map; only a successful
    /// GPU present commits it into `PaneState`.
    frame_pane_output_generations: HashMap<PaneId, u64>,
    track_pane_output_generations_for_frame: bool,
    pane_font_cache: RefCell<HashMap<PaneFontKey, PaneFontEntry>>,
    pane_font_cache_tick: Cell<u64>,
    /// One recorded thumbnail per card, kept across frames so an unchanged
    /// terminal is replayed rather than re-shaped and re-quadded. Entries are
    /// dropped for cards that stop asking for a preview, and invalidated
    /// wholesale when the glyph atlas is repacked -- the quads hold atlas
    /// coordinates.
    preview_quad_cache:
        RefCell<HashMap<TabId, crate::termwindow::render::paint::CachedPreviewQuads>>,
    /// The one card rebuild currently sliced across frames, if any.
    preview_rebuild_partial:
        RefCell<Option<crate::termwindow::render::paint::PreviewRebuildPartial>>,
    /// When each card's terminal first showed visible content, keyed like
    /// `preview_quad_cache`. Drives the blank->content fade-in; survives
    /// overview closes and atlas repacks so neither replays the fade.
    preview_content_fade:
        RefCell<HashMap<TabId, crate::termwindow::render::paint::PreviewContentFade>>,
    /// Cards whose heap must be rendered into their texture this frame;
    /// queued by the paint pass, encoded by draw before the main pass.
    pending_card_renders: RefCell<Vec<crate::termwindow::render::paint::PendingCardRender>>,
    /// Flattened vertices for every pending card render and composite quad of
    /// the current frame, in one persistent grow-only buffer. Cleared (not
    /// dropped) each pass so rebuilds stop allocating megabytes per card.
    card_frame_verts: RefCell<Vec<crate::quad::Vertex>>,
    /// One textured quad per card standing in for its glyph quads.
    card_composites: RefCell<Vec<crate::termwindow::render::paint::CardComposite>>,
    /// Per-cell quads of pictures drawn from dedicated textures this pass
    /// (see `populate_image_quad`), consumed by the composite passes.
    image_composites: RefCell<crate::termwindow::render::paint::ImageCompositeBatch>,
    /// Set by `populate_image_quad` when the line being rendered drew a
    /// picture from a dedicated texture; the pane renderer then keeps that
    /// line out of the line quad cache, whose replay would lose the picture.
    dedicated_image_in_line: std::cell::Cell<bool>,
    /// Smooth scrolling: the vertical shift the line replay applies while
    /// a pane is scrolled by a fraction of a row, for painters that bypass
    /// the line layers (dedicated image composites). 0 outside the loop.
    line_render_y_offset: std::cell::Cell<f32>,
    /// The (layer, cell) whose latest picture went to a dedicated texture,
    /// so pictures stacked above it in the same cell follow it there and
    /// keep their z-order; reset at the start of every line.
    dedicated_image_cell: std::cell::Cell<Option<(usize, usize)>>,
    /// The settled frame's composites, kept so a closing overview's ghost
    /// can fade the card pictures out with it (the ghost heap itself holds
    /// no thumbnail quads on the texture path).
    content_view_last_composites: RefCell<Vec<crate::termwindow::render::paint::CardComposite>>,
    /// Grow-only scratch GPU buffers for card render passes.
    card_scratch: RefCell<Option<crate::termwindow::render::draw::CardScratch>>,
    /// Font scale last chosen for a thumbnail of a grid this size, so a drag
    /// can hold it steady instead of rebucketing every few pixels.
    preview_scale_hold: RefCell<HashMap<(usize, usize), f64>>,
    semantic_zones: HashMap<PaneId, SemanticZoneCache>,

    window_background: Vec<LoadedBackgroundLayer>,

    current_modifier_and_leds: (Modifiers, KeyboardLedStatus),
    current_mouse_buttons: Vec<MousePress>,
    current_mouse_capture: Option<MouseCapture>,

    opengl_info: Option<String>,

    /// The frame a restored main window is meant to occupy, until the startup
    /// resizes have settled on it. See `reassert_restored_frame`.
    restored_frame_target: Option<ScreenRect>,
    /// How many more times that correction may be applied before giving up, so
    /// a backend that will not take the frame cannot be fought forever.
    restored_frame_attempts: u8,
    /// When to stop correcting regardless. Startup does not always disturb the
    /// frame -- a window that reopens at a size the GUI is happy with is left
    /// alone -- and without a deadline the correction would still be armed
    /// much later, when the first thing it "corrected" would be the user
    /// resizing their own window.
    restored_frame_deadline: Option<Instant>,

    /// Keeps track of double and triple clicks
    last_mouse_click: Option<LastMouseClick>,

    /// The URL over which we are currently hovering
    current_highlight: Option<Arc<Hyperlink>>,

    quad_generation: usize,
    /// The chrome's colours, resolved once per configuration rather than once
    /// per draw. Deriving them is cheap, but the appearance they are derived
    /// from was not: `native_settings::effective_appearance()` deep-clones the
    /// whole settings struct and asks the platform for its appearance, and the
    /// paint path was calling it nineteen times a frame.
    ///
    /// Refreshed in `config_was_reloaded`, which is also where a colour scheme
    /// override and an appearance change both land.
    chrome_palette: crate::ui::UiPalette,
    shape_generation: usize,
    shape_cache: RefCell<LfuCache<ShapeCacheKey, anyhow::Result<Rc<Vec<ShapedInfo>>>>>,
    /// Per-domain shaping caches for proportional UI text (chrome / Note /
    /// File Preview), separate from the terminal's `shape_cache`. Sharing one
    /// LFU meant a long note's thousands of run strings re-shaped every fling
    /// on the UI thread AND evicted the terminal's and chrome's entries; the
    /// split also gives each domain a byte budget and independent idle
    /// release.
    ui_shape_caches: RefCell<crate::shapecache::UiShapeCaches>,
    /// Which domain cache `cached_ui_shape` routes to; set by the Note and
    /// File Preview paint entry points, Chrome otherwise.
    ui_text_domain: std::cell::Cell<crate::shapecache::UiTextDomain>,
    /// Throttles the periodic paint-time diagnostics publish; clears and
    /// releases publish immediately regardless.
    last_ui_shape_diagnostics_publish: std::cell::Cell<Option<Instant>>,
    line_to_ele_shape_cache: RefCell<LfuCache<LineToEleShapeCacheKey, LineToElementShapeItem>>,

    line_state_cache: RefCell<LfuCacheU64<Arc<CachedLineState>>>,
    next_line_state_id: u64,

    line_quad_cache: RefCell<LfuCache<LineQuadCacheKey, LineQuadCacheValue>>,
    /// Scratch recorder every line's quads go into before they are moved
    /// into `line_quad_cache` at exact size. A fresh
    /// `HeapQuadAllocator::default()` per cache miss reached a line's ~25 KiB
    /// of quads by doubling from zero: measured 1.5 GiB of alloc/copy/free
    /// in 15 minutes of a 4 Hz full-screen TUI, with the doubling slack
    /// then kept resident by the cache. One reused recorder makes the
    /// steady state allocation-free apart from the exact-size copy that
    /// becomes the entry.
    ///
    /// Exactly one line is recorded at a time: `LineRender::render_line` is
    /// the only borrower and nothing under `render_screen_line` re-enters
    /// it, so a double borrow would be a real bug and should panic.
    line_quad_scratch: RefCell<HeapQuadAllocator>,

    last_status_call: Instant,
    /// Throttle for `log_gpu_allocator_throttled`; runtime-only diagnostics.
    last_gpu_allocator_log: Cell<Option<Instant>>,
    cursor_blink_state: RefCell<ColorEase>,
    blink_state: RefCell<ColorEase>,
    rapid_blink_state: RefCell<ColorEase>,

    palette: Option<ColorPalette>,

    ui_items: Vec<UIItem>,
    context_menu: Option<ui::context_menu::ContextMenuState>,
    command_palette: Option<ui::command_palette::CommandPaletteState>,
    /// The contrast floor text is held to, or `None` to leave the
    /// application's colours alone. Resolved from the settings window's
    /// choice and the configuration file once per reload rather than per
    /// cell; see `native_settings::text_min_contrast_ratio`.
    text_min_contrast: Option<f32>,
    /// Adjusted foregrounds for the contrast floor, keyed on the
    /// (foreground, background, ratio) that produced them. See
    /// `ensure_min_contrast`.
    min_contrast_memo: RefCell<HashMap<[u32; 9], LinearRgba>>,
    /// The terminal background of a scheme being previewed, so the chrome can
    /// follow a pick before it is made. `None` outside a preview.
    scheme_preview_ground: Option<LinearRgba>,
    /// The colour scheme the command palette is showing but the user has not
    /// chosen: `Some(None)` the configured default, `Some(Some(name))` a named
    /// scheme, `None` no preview. Lives here rather than on the palette state
    /// because every path that closes the palette has already taken that state
    /// away, and closing is exactly when the preview must be undone.
    scheme_preview: Option<Option<String>>,
    context_menu_application_actions: HashMap<u64, ContextMenuApplicationAction>,
    next_context_menu_application_action_id: u64,
    context_menu_suppressed_release: Option<MousePress>,
    dragging: Option<(UIItem, MouseEvent)>,
    // In-flight drag of a Files-panel row toward the terminal; becomes
    // active once the pointer moves past a small threshold so plain clicks
    // still open the file.
    right_sidebar_file_drag: Option<FileDragState>,
    /// In-flight drag of a level-2 pane tab toward a move/split drop.
    pane_tab_drag: Option<PaneTabDragState>,
    /// In-flight reorder drag of a left-sidebar Project or thread row.
    sidebar_row_drag: Option<SidebarRowDragState>,
    /// Content views (e.g. SSH hosts) shown as synthetic tabs.
    content_views: Vec<ContentViewTab>,
    active_content_view_id: Option<ContentViewId>,
    content_view_response_tab_id: Option<ContentViewId>,
    /// A real terminal-size change arrived while a ContentView owned the
    /// foreground.  It is flushed once when terminal content becomes visible
    /// again; merely opening and closing a view must not resize mux tabs.
    content_view_deferred_mux_resize: bool,
    /// A full-window view arriving or leaving. `None` outside a transition,
    /// which is the state the paint pass treats as "one world or the other".
    content_view_fade: Option<crate::termwindow::content_view::ContentViewFade>,
    /// The most recent full-window view frame, recorded so that closing one
    /// has a picture to take away after the view itself is gone.
    content_view_last_frame: Option<crate::quad::HeapQuadAllocator>,
    next_content_view_id: ContentViewId,
    registered_content_view_surfaces: HashMap<ContentViewId, MuxWindowId>,
    /// Tracks SSH connections kicked off by `RemoteThreadView`s.
    remote_connects: HashMap<ContentViewId, RemoteConnectState>,
    next_remote_connect_generation: u64,
    /// Tracks Mosh bootstraps kicked off by `RemoteThreadView`s.
    mosh_connects: HashMap<ContentViewId, MoshConnectState>,
    next_mosh_connect_generation: u64,
    local_thread_activation: Option<LocalThreadActivationState>,
    next_local_thread_activation_generation: u64,
    space_owner_id: u64,
    active_space_id: String,
    /// True for windows the user did not explicitly open (domain-owned
    /// windows spawned by the reconcile, e.g. reconnect/auth prompts): they
    /// close when their mux window dies instead of falling back to a thread.
    dies_with_mux_window: bool,
    /// Domains with a user-requested reconnect currently in flight (the
    /// sidebar Reconnect button); suppresses double-clicks and drives the
    /// row's "Connecting…" label.
    space_reconnects_in_flight: HashSet<String>,
    workspace_layout_structure_fingerprint: Option<u64>,
    workspace_sidebar_width: usize,
    workspace_sidebar_pending_thread_selection: Option<String>,
    workspace_sidebar_collapsed: bool,
    workspace_sidebar_scroll_offset: f32,
    /// Where each Space's sidebar was scrolled to when the window last left
    /// it. Switching used to send every Space back to the top, so glancing at
    /// a neighbour cost you your place in a long list.
    workspace_sidebar_scroll_offsets: HashMap<String, f32>,
    workspace_sidebar_swipe: space_swipe::SidebarSpaceSwipeState,
    /// A CPU-side copy of the source Space's left sidebar. During a committed
    /// switch only its middle project/thread list viewport is composited beside
    /// the destination; sidebar chrome and the rest of the window stay live.
    workspace_space_swipe_source_frame: Option<CapturedSidebar>,
    workspace_space_swipe_capture_source: bool,
    workspace_space_swipe_push_active: bool,
    workspace_space_swipe_direction: f32,
    workspace_space_swipe_pending_commit: Option<(String, f32)>,
    workspace_space_swipe_needs_settle_start: bool,
    /// The Space whose sidebar is being painted right now, when that is not
    /// the adopted one. A swipe paints the neighbouring Space into an
    /// offscreen buffer so it can slide in under the finger; this is how that
    /// paint reads the neighbour's projects and threads without the window
    /// actually switching to it.
    workspace_sidebar_preview_space_id: Option<String>,
    /// The neighbouring Space's sidebar, and which Space it holds. Captured
    /// once when the axis locks rather than every frame: it is not
    /// interactive while the finger is down, and re-rasterising its glyphs
    /// per frame would cost more than the whole animation budget.
    workspace_space_swipe_target_frame: Option<(String, CapturedSidebar)>,
    /// Whether any frame was composited with the pages following the finger.
    /// A flick that begins and ends between two paints never gets one, and
    /// its transition has to open at rest to be seen at all.
    workspace_space_swipe_tracked: bool,
    /// Where the scrolling list began and ended in the most recent
    /// heap-recorded sidebar paint. `paint_workspace_sidebar` clears this on
    /// entry and sets it around the row loop, so it describes that paint and
    /// no other; a paint that bailed out before the list leaves it `None`.
    workspace_sidebar_list_quads: Option<(HeapQuadMark, HeapQuadMark)>,
    workspace_sidebar_scrollbar_visible_until: Option<Instant>,
    /// Presentation-only hover reveal of the collapsed sidebar. This never
    /// touches `workspace_sidebar_collapsed` or `workspace_sidebar_width`:
    /// the terminal must not reflow for a hover.
    workspace_sidebar_hover: sidebar_hover::SidebarHoverReveal,
    /// The pointer is over the native macOS titlebar sidebar button (which
    /// lives outside our view, so `current_mouse_event` cannot see it).
    /// Feeds the hover-reveal as a hot zone.
    titlebar_sidebar_button_hovered: bool,
    /// Collapsed folder-style reference groups, keyed by
    /// `"<space_id>::<group_key>"`, where the group key is the origin
    /// project id (or a machine fallback for dangling groups). Runtime-only:
    /// collapse state resets
    /// with the window, like a disclosure and unlike project collapse.
    thread_ref_groups_collapsed: std::collections::HashSet<String>,
    /// Transient reveal of archived projects in the sidebar. Deliberately
    /// NOT persisted: "archived" means hidden, and a setting left on would
    /// quietly un-implement the feature. Resets with the window.
    pub(crate) workspace_sidebar_show_archived: bool,
    /// A native (AppKit) context menu is open. The fallback menu tracks
    /// itself in `context_menu`; the native path otherwise leaves no trace,
    /// and the hover machine must not retreat the panel a menu is anchored
    /// to.
    native_context_menu_open: bool,
    /// Last notification state painted by this GUI window. Comparing identities
    /// and statuses (rather than just the count) lets the bell pulse when one
    /// notification replaces another without changing the total.
    workspace_notification_snapshot:
        Option<HashMap<String, crate::workspace_threads::WorkspaceThreadWorkStatus>>,
    workspace_notification_pulse_started_at: Option<Instant>,
    right_sidebar_width: usize,
    right_sidebar_collapsed: bool,
    right_sidebar_mode: RightSidebarMode,
    right_sidebar_agents_scroll: f32,
    right_sidebar_snippet_view: RightSidebarSnippetView,
    right_sidebar_snippet_focus: Option<RightSidebarSnippetField>,
    right_sidebar_snippet_search: TextInputState,
    right_sidebar_snippet_title: TextInputState,
    right_sidebar_snippet_body: TextInputState,
    right_sidebar_snippet_scroll_offset: f32,
    right_sidebar_snippet_scrollbar_visible_until: Option<Instant>,
    right_sidebar_note: crate::markdown_editor::NoteHostState,
    right_sidebar_note_view: RightSidebarNoteView,
    right_sidebar_note_vault_index_root: Option<PathBuf>,
    right_sidebar_note_vault_paths: Arc<Vec<String>>,
    right_sidebar_note_vault_index_generation: u64,
    right_sidebar_note_vault_indexing: bool,
    right_sidebar_note_vault_last_scan: Option<Instant>,
    right_sidebar_note_open_generation: u64,
    right_sidebar_note_opening: Option<(PathBuf, String)>,
    /// The last open that failed, and for which document. Carries the FULL
    /// classified failure, not just its text: the replay below re-establishes
    /// the problem page from it, so a scan that recovers on its own cannot
    /// leave a still-broken note showing a dead-end message with no way out.
    right_sidebar_note_open_failure:
        Option<((PathBuf, String), ui::right_sidebar::NoteVaultFailure)>,
    /// Set only for a REAL failure to open the vault, never for the transient
    /// "Opening note..." / "Indexing Vault..." states that also live in
    /// `load_error`: its presence is what swaps the panel for the problem page
    /// and its escape-hatch buttons. It carries its own detail text rather than
    /// reading `load_error`, which a later transient would overwrite.
    right_sidebar_note_vault_failure: Option<ui::right_sidebar::NoteVaultFailure>,
    right_sidebar_note_tree_scroll_offset: f32,
    right_sidebar_note_tree_expanded: HashSet<String>,
    right_sidebar_note_vault_tree_collapsed: bool,
    right_sidebar_note_wide_layout: bool,
    right_sidebar_note_table_horizontal_offsets: std::collections::BTreeMap<usize, f32>,
    right_sidebar_note_table_layouts: Vec<RightSidebarNoteTableLayout>,
    right_sidebar_note_images: HashMap<RightSidebarNoteImageSource, RightSidebarFilePreviewImage>,
    right_sidebar_note_images_loading: HashSet<RightSidebarNoteImageSource>,
    right_sidebar_note_image_order: VecDeque<RightSidebarNoteImageSource>,
    right_sidebar_note_image_failures: HashMap<RightSidebarNoteImageSource, Instant>,
    right_sidebar_note_code_highlight: ui::right_sidebar::NoteCodeHighlightState,
    right_sidebar_note_paint_cache: ui::right_sidebar::NotePaintCache,
    right_sidebar_note_prewarm: Option<ui::right_sidebar::NotePrewarmState>,
    right_sidebar_note_memory_release_token: u64,
    right_sidebar_file_view: RightSidebarFileView,
    right_sidebar_file_focus: Option<RightSidebarFileField>,
    right_sidebar_file_filter: TextInputState,
    right_sidebar_file_applied_filter: String,
    right_sidebar_file_filter_debounce_until: Option<Instant>,
    right_sidebar_file_expanded: HashSet<String>,
    right_sidebar_file_expanded_version: u64,
    right_sidebar_file_index_generation: u64,
    right_sidebar_file_index_root: Option<PathBuf>,
    right_sidebar_file_index_project_name: String,
    right_sidebar_file_index_status: RightSidebarFileIndexStatus,
    right_sidebar_file_index: Option<Arc<RightSidebarFileIndex>>,
    right_sidebar_file_index_cancel: Option<Arc<AtomicBool>>,
    /// Lazily-read directories backing the browse tree. Independent of the
    /// search index above, which is only built once the user actually searches.
    right_sidebar_file_dir_cache: RightSidebarFileDirCache,
    /// Directories with a `read_dir` in flight, so a folder that stays expanded
    /// across several frames is not re-read on every paint.
    right_sidebar_file_dir_loads_in_flight: HashSet<PathBuf>,
    /// Invalidates in-flight directory loads whose root/project no longer match.
    right_sidebar_file_dir_cache_generation: u64,
    /// Set when the idle release tears the panel down, so the next paint knows
    /// to restore the saved view (notably the preview it dropped). The index
    /// status used to stand in for this, which stops working once the index is
    /// only built on demand and legitimately stays `Empty`.
    right_sidebar_file_view_needs_restore: bool,
    // Bumped each time the file panel goes idle; a delayed release task only
    // frees the index/buffers if its captured token still matches (i.e. the
    // panel was not reopened or re-toggled in the meantime).
    right_sidebar_file_memory_release_token: u64,
    right_sidebar_file_search_generation: u64,
    right_sidebar_file_search_cancel: Option<Arc<AtomicBool>>,
    right_sidebar_file_search_query: String,
    right_sidebar_file_search_rows: Vec<RightSidebarFileTreeRow>,
    right_sidebar_file_searching: bool,
    right_sidebar_file_browse_rows: Vec<RightSidebarFileTreeRow>,
    right_sidebar_file_browse_cache_key: Option<(u64, u64)>,
    right_sidebar_file_selected: Option<PathBuf>,
    right_sidebar_file_tree_width: usize,
    right_sidebar_file_preview_width: usize,
    right_sidebar_note_pane_expanded: bool,
    right_sidebar_note_pane_width: usize,
    /// Cached `space_note_vault(active_space_id).is_some()` so the hot layout
    /// predicate does not lock the workspace store; refreshed on space/vault
    /// changes and self-healed once per Note paint.
    active_space_has_note_vault: bool,
    /// Sidebar width from before a Space switch whose reflow must wait until
    /// the destination mux window is adopted (`switch_to_mux_window`), so the
    /// resize/SIGWINCH never hits the Space being left.
    pending_sidebar_reflow_width: Option<usize>,
    right_sidebar_file_preview_generation: u64,
    right_sidebar_file_preview_highlight_cancel: Arc<AtomicUsize>,
    right_sidebar_file_preview_lines: Vec<RightSidebarFilePreviewLine>,
    /// The preview text before display sanitization (tab expansion, control
    /// stripping): the display lines are wrong for the clipboard.
    right_sidebar_file_preview_raw_text: Option<String>,
    right_sidebar_file_preview_max_columns: usize,
    right_sidebar_file_preview_image: Option<RightSidebarFilePreviewImage>,
    right_sidebar_file_preview_message: Option<String>,
    right_sidebar_file_preview_truncated: bool,
    right_sidebar_file_preview_selection: Option<RightSidebarFilePreviewSelection>,
    right_sidebar_file_preview_slice_cache: RefCell<
        HashMap<RightSidebarFilePreviewSliceCacheKey, RightSidebarFilePreviewSliceCacheValue>,
    >,
    right_sidebar_file_preview_slice_cache_order:
        RefCell<VecDeque<RightSidebarFilePreviewSliceCacheKey>>,
    // Full-line per-character colours for the horizontal-scroll fast path, keyed
    // by (preview generation, line index). Built once per line and reused across
    // every horizontal offset so panning never rebuilds the colour list.
    right_sidebar_file_preview_line_color_cache:
        RefCell<HashMap<(u64, usize), Rc<Vec<LinearRgba>>>>,
    right_sidebar_file_preview_line_color_cache_order: RefCell<VecDeque<(u64, usize)>>,
    right_sidebar_file_tree_scroll_offset: f32,
    right_sidebar_file_preview_scroll_offset: f32,
    right_sidebar_file_preview_horizontal_offset: usize,
    // When restoring a remembered preview, the async load result resets the
    // scroll to 0; this carries the offsets to re-apply once the lines arrive.
    right_sidebar_file_preview_restore_scroll: Option<(f32, usize)>,
    // Per-(root, project) remembered Files-panel view state (expanded folders,
    // selected/preview, scroll, filter) so switching workspaces / idle-release /
    // re-scan don't lose where you were. Only a few KB of paths each; LRU-bounded.
    right_sidebar_file_view_state_by_root: HashMap<(PathBuf, String), RightSidebarFileViewState>,
    right_sidebar_file_view_state_order: VecDeque<(PathBuf, String)>,
    right_sidebar_remote_files: remote_files::RemoteFilesState,
    right_sidebar_remote_files_lease: Option<remote_files::RemoteConnectionLease>,
    /// Transfers in flight, plus recently finished ones still worth showing.
    /// Deliberately not part of `right_sidebar_remote_files`: a transfer holds
    /// its own lease and must outlive the panel switching between trees.
    right_sidebar_remote_transfers: Vec<remote_files::RemoteTransfer>,
    right_sidebar_remote_transfer_next_id: u64,
    /// The directory a file drag is currently hovering over, so the row can be
    /// highlighted and the drop knows where it would land.
    right_sidebar_remote_drop_target: Option<remote_files::RemotePath>,
    right_sidebar_local_drop_target: Option<PathBuf>,
    /// A local copy waiting on the user's answer about existing files.
    pending_local_copy: Option<PendingLocalCopy>,
    /// A remote operation waiting on its confirmation menu. Superseded (and
    /// cancelled) by any newer confirmation, and cleared when its menu is
    /// dismissed without an answer.
    pending_remote_confirm: Option<PendingRemoteConfirm>,
    /// A freshly created remote folder that should drop into an inline rename
    /// as soon as the re-listed directory actually shows its row.
    pending_remote_rename: Option<(
        remote_files::RemotePath,
        remote_files::RemoteOperationOrigin,
    )>,
    /// Bumped by every drop. A preflight walk that comes back carrying an old
    /// value has been superseded — two walks can finish out of order, and the
    /// slower, older one must not replace the newer prompt.
    local_copy_generation: u64,
    /// How many destinations clashed, kept separately so the prompt can be
    /// worded without borrowing the queued plan.
    pending_local_copy_conflict_count: usize,
    right_sidebar_remote_file_tree_scroll_offset: f32,
    // Bumped to invalidate a pending periodic-rescan timer tick.
    right_sidebar_file_rescan_token: u64,
    // True while a keep-showing refresh build is in flight (drives the Refresh
    // button spinner); the old tree stays visible meanwhile.
    right_sidebar_file_refreshing: bool,
    right_sidebar_open_with_generation: u64,
    right_sidebar_open_with_cache: HashMap<String, RightSidebarOpenWithCacheEntry>,
    right_sidebar_open_with_app: Option<crate::native_settings::NativeOpenWithApp>,
    right_sidebar_input_layouts: Vec<RightSidebarInputLayout>,

    modal: RefCell<Option<Rc<dyn Modal>>>,

    event_states: HashMap<String, EventState>,
    pub current_event: Option<Value>,
    has_animation: RefCell<Option<Instant>>,
    /// We use this to attempt to do something reasonable
    /// if we run out of texture space
    allow_images: AllowImage,
    /// Image downscale level carried across frames after an atlas overflow,
    /// and when it was armed. See [`ATLAS_SCALE_HOLD`].
    atlas_scale_hold: Option<(AllowImage, Instant)>,
    /// Recent atlas overflow events, oldest first, for the memory report.
    atlas_overflow_log: std::collections::VecDeque<AtlasOverflowRecord>,
    scheduled_animation: RefCell<Option<Instant>>,

    /// Single-flight latch for the unfocused repaint throttle: the
    /// deadline of the in-flight trailing-edge timer, if any.
    unfocused_invalidate_due: Option<Instant>,
    /// The earliest moment the next output-driven repaint of this window
    /// may happen while it is unfocused.
    unfocused_next_allowed: Instant,

    created: Instant,

    pub last_frame_duration: Duration,
    last_fps_check_time: Instant,
    num_frames: usize,
    pub fps: f32,

    connection_name: String,

    gl: Option<Rc<glium::backend::Context>>,
    webgpu: Option<Rc<WebGpuState>>,
    config_subscription: Option<config::ConfigSubscription>,
}

impl TermWindow {
    /// Whether pictures may draw from dedicated textures right now. A
    /// content-view transition records the whole terminal into one heap and
    /// replays it scaled; per-cell composites cannot be replayed from a
    /// heap, so those frames fall back to the atlas path.
    pub(crate) fn dedicated_image_textures_allowed(&self) -> bool {
        self.content_view_fade.is_none() && self.content_view_last_frame.is_none()
    }

    /// What the window was showing when the atlas overflowed; the report
    /// uses it to tell an overview spike from a plain terminal frame.
    pub(crate) fn atlas_scene(&self) -> &'static str {
        if self.content_view_transition_running() {
            "transition"
        } else if self.content_view_foreground() {
            "content_view"
        } else {
            "terminal"
        }
    }

    pub(crate) fn note_atlas_overflow(
        &mut self,
        pass: usize,
        have: usize,
        want: usize,
        action: &'static str,
    ) {
        let scene = self.atlas_scene();
        if self.atlas_overflow_log.len() >= ATLAS_OVERFLOW_LOG_LEN {
            self.atlas_overflow_log.pop_front();
        }
        self.atlas_overflow_log.push_back(AtlasOverflowRecord {
            at: Instant::now(),
            pass,
            have,
            want,
            action,
            scene,
        });
    }

    /// Emit the GPU allocator breakdown to the log at most once every 15s.
    ///
    /// The settings panel shows the same thing, but a scripted measurement
    /// cannot open the settings panel -- and opening it adds a second window
    /// with its own device and swapchain, which perturbs exactly the numbers
    /// being measured.
    pub(crate) fn log_gpu_allocator_throttled(&self) {
        const INTERVAL: Duration = Duration::from_secs(15);
        let now = Instant::now();
        if let Some(last) = self.last_gpu_allocator_log.get() {
            if now.saturating_duration_since(last) < INTERVAL {
                return;
            }
        }
        self.last_gpu_allocator_log.set(Some(now));
        if let Some(webgpu) = self.webgpu.as_ref() {
            for line in gpu_allocator_lines(&webgpu.device, "perf") {
                log::info!("thinkterm_perf {line}");
            }
            // Diagnostic probe: does blocking until the GPU is idle release
            // the staging buffers? If it does, they were merely in flight
            // (the queue was outrunning the device); if it does not, nothing
            // is going to free them.
            if std::env::var_os("THINKTERM_GPU_WAIT_PROBE").is_some() {
                let started = Instant::now();
                let status = webgpu.device.poll(wgpu::PollType::Wait);
                log::info!(
                    "thinkterm_perf perf: gpu_wait_probe status={:?} took={:?}",
                    status,
                    started.elapsed()
                );
                for line in gpu_allocator_lines(&webgpu.device, "after_wait") {
                    log::info!("thinkterm_perf {line}");
                }
            }
        }
    }

    pub(crate) fn memory_resource_lines(&self, label: &str) -> Vec<String> {
        let backend = if self.webgpu.is_some() {
            "WebGpu"
        } else if self.gl.is_some() {
            "OpenGL"
        } else {
            "none"
        };
        let tab_count = Mux::get()
            .get_window(self.mux_window_id)
            .map(|window| window.len())
            .unwrap_or(0);
        let mut lines = vec![format!(
            "{label}: backend={backend} size={}x{} dpi={} panes={} tabs={}",
            self.dimensions.pixel_width,
            self.dimensions.pixel_height,
            self.dimensions.dpi,
            self.get_panes_to_render().len(),
            tab_count
        )];

        // Which adapter we actually got decides how to read every other number
        // in this report: on a discrete GPU the swapchain and textures are VRAM
        // and appear in neither working set nor commit, on an integrated one
        // they are system memory, and on a Cpu adapter (WARP) everything is.
        if let Some(webgpu) = self.webgpu.as_ref() {
            let info = &webgpu.adapter_info;
            lines.push(format!(
                "{label}: adapter name={} device_type={:?} backend={:?} driver={} driver_info={}",
                info.name, info.device_type, info.backend, info.driver, info.driver_info,
            ));
            for line in gpu_allocator_lines(&webgpu.device, label) {
                lines.push(line);
            }
        }

        if let Some(render_state) = self.render_state.as_ref() {
            let stats = render_state.stats();
            lines.push(format!(
                "{label}: render_backend={} atlas={} glyphs={} svg_icons={} rotated_icons={} images={} frames={} blocks={} colors={} cursor_glyphs={}",
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
                "{label}: layers={} vertex_buffers={} quad_capacity={} line_glyphs={}",
                stats.layers, stats.vertex_buffers, stats.layer_quads, stats.line_glyphs
            ));
            // RGBA texels -> MiB. Packed area is a high-water mark since the
            // last clear (the allocator never frees a rectangle), so
            // "used" means "was packed", not "is on screen".
            let mib = |px: u64| px as f64 * 4.0 / (1024.0 * 1024.0);
            let usage = stats.atlas_usage;
            let capacity_px = (stats.atlas_size * stats.atlas_size) as u64;
            let percent = if capacity_px == 0 {
                0.0
            } else {
                usage.allocated_px as f64 * 100.0 / capacity_px as f64
            };
            let tag = |t: ::window::bitmaps::atlas::AtlasTag| {
                let u = usage.tag(t);
                format!("{:.1}MiB/{}", mib(u.allocated_px), u.allocations)
            };
            lines.push(format!(
                "{label}: atlas_packed={:.1}MiB/{:.0}MiB ({percent:.0}%) glyph={} image={} other(icons/blocks/lines/util)={} max_rect={}x{} failures={}",
                mib(usage.allocated_px),
                mib(capacity_px),
                tag(::window::bitmaps::atlas::AtlasTag::Glyph),
                tag(::window::bitmaps::atlas::AtlasTag::Image),
                tag(::window::bitmaps::atlas::AtlasTag::Other),
                usage.max_rect.0,
                usage.max_rect.1,
                usage.failures,
            ));
            lines.push(format!(
                "{label}: recent_images=[{}]",
                stats.recent_image_allocs.join("; ")
            ));
            let (remote_images, remote_image_bytes) =
                wezterm_client::pane::remote_image_footprint();
            lines.push(format!(
                "{label}: remote_images={} {:.1}MiB",
                remote_images,
                remote_image_bytes as f64 / (1024.0 * 1024.0),
            ));
            let dedicated = stats.dedicated_images;
            lines.push(format!(
                "{label}: dedicated_images live={} pooled={} {:.1}MiB budget={}MiB",
                dedicated.live,
                dedicated.pooled,
                dedicated.bytes as f64 / (1024.0 * 1024.0),
                crate::renderstate::DEDICATED_IMAGE_BUDGET_BYTES / (1024 * 1024),
            ));
        } else {
            lines.push(format!("{label}: render_state=none"));
        }
        {
            let now = Instant::now();
            let recent: Vec<String> = self
                .atlas_overflow_log
                .iter()
                .rev()
                .take(8)
                .map(|r| {
                    format!(
                        "{}s:p{} {}->{} {} {}",
                        now.saturating_duration_since(r.at).as_secs(),
                        r.pass,
                        r.have,
                        r.want,
                        r.action,
                        r.scene
                    )
                })
                .collect();
            lines.push(format!(
                "{label}: atlas_overflows={} scale_hold={:?} recent=[{}]",
                self.atlas_overflow_log.len(),
                self.atlas_scale_hold.map(|(level, _)| level),
                recent.join("; ")
            ));
        }

        lines.push(format!(
            "{label}: caches shape={} ({}KiB) line_state={} ({}KiB) line_quad={} ({}KiB) line_to_element_shape={} ({}KiB) pane_font={} semantic_zones={}",
            self.shape_cache.borrow().len(),
            self.shape_cache.borrow().total_weight() / 1024,
            self.line_state_cache.borrow().len(),
            self.line_state_cache.borrow().total_weight() / 1024,
            self.line_quad_cache.borrow().len(),
            self.line_quad_cache.borrow().total_weight() / 1024,
            self.line_to_ele_shape_cache.borrow().len(),
            self.line_to_ele_shape_cache.borrow().total_weight() / 1024,
            self.pane_font_cache.borrow().len(),
            self.semantic_zones.len(),
        ));
        {
            let ui = self.ui_shape_caches.borrow();
            lines.push(format!(
                "{label}: ui_shape_caches chrome={} ({}KiB) note={} ({}KiB) file_preview={} ({}KiB)",
                ui.domain(crate::shapecache::UiTextDomain::Chrome).len(),
                ui.domain(crate::shapecache::UiTextDomain::Chrome)
                    .total_weight()
                    / 1024,
                ui.domain(crate::shapecache::UiTextDomain::Note).len(),
                ui.domain(crate::shapecache::UiTextDomain::Note).total_weight() / 1024,
                ui.domain(crate::shapecache::UiTextDomain::FilePreview).len(),
                ui.domain(crate::shapecache::UiTextDomain::FilePreview)
                    .total_weight()
                    / 1024,
            ));
        }
        lines
    }

    fn load_os_parameters(&mut self) {
        if let Some(ref window) = self.window {
            self.os_parameters = match window.get_os_parameters(&self.config, self.window_state) {
                Ok(os_parameters) => os_parameters,
                Err(err) => {
                    log::warn!("Error while getting OS parameters: {:#}", err);
                    None
                }
            };
        }
    }

    fn remember_workspace_layout_structure_fingerprint(&mut self) {
        self.workspace_layout_structure_fingerprint =
            crate::workspace_threads::window_layout_structure_fingerprint(self.mux_window_id);
    }

    fn persist_workspace_layout_if_structure_changed(&mut self) {
        let now = crate::workspace_threads::window_layout_structure_fingerprint(self.mux_window_id);
        if now.is_some() && now != self.workspace_layout_structure_fingerprint {
            self.persist_workspace_layout_after_mutation("tab structure changed");
        }
    }

    pub(crate) fn persist_workspace_layout_after_mutation(&mut self, reason: &'static str) {
        if let Some(workspace) = self.current_mux_workspace() {
            if crate::workspace_threads::is_materializing_thread_layout(&workspace) {
                return;
            }
        }

        log::trace!("snapshot workspace thread layout after {reason}");
        self.snapshot_active_workspace_thread_layout();
        self.remember_workspace_layout_structure_fingerprint();
    }

    fn should_preserve_mux_window_on_gui_close(&self) -> bool {
        let mux = Mux::get();
        let Some((origin_domain, pane_domains, client_pane_domains)) =
            mux.get_window(self.mux_window_id).map(|window| {
                let panes = window
                    .iter()
                    .flat_map(|tab| tab.iter_all_panes())
                    .collect::<Vec<_>>();
                let pane_domains = panes
                    .iter()
                    .map(|pane| pane.domain_id())
                    .collect::<Vec<_>>();
                let client_pane_domains = panes
                    .iter()
                    .filter(|pane| {
                        pane.downcast_ref::<wezterm_client::pane::ClientPane>()
                            .is_some()
                    })
                    .map(|pane| pane.domain_id())
                    .collect::<Vec<_>>();
                (window.origin_domain(), pane_domains, client_pane_domains)
            })
        else {
            return false;
        };

        // The local session host counts as a client domain here too: its
        // terminals live in the session server, and closing a window is
        // closing a view of them, as it is for a remote host. Closing a tab
        // or a pane still ends what runs in it.
        let is_client_domain = |domain: &Arc<dyn mux::domain::Domain>| {
            domain.downcast_ref::<ClientDomain>().is_some()
        };
        let origin_client_domain = origin_domain
            .and_then(|domain_id| mux.get_domain(domain_id))
            .filter(is_client_domain)
            .map(|domain| domain.domain_id());

        let active_space_domain =
            crate::workspace_threads::client_domain_for_space(&self.active_space_id)
                .and_then(|domain_name| mux.get_domain_by_name(&domain_name))
                .filter(is_client_domain)
                .map(|domain| domain.domain_id());
        // A window the layout restore made for the host carries no origin
        // tag (`spawn_tab_or_window` tags nothing), so judge it by what it
        // holds: every pane a mirror of the host's.
        let host_pane_domain = client_pane_domains
            .first()
            .copied()
            .filter(|domain_id| crate::local_sessions::is_host_domain_id(*domain_id));

        preserve_mux_window_on_gui_close(
            origin_client_domain,
            active_space_domain.or(host_pane_domain),
            &pane_domains,
            &client_pane_domains,
        )
    }

    pub(crate) fn close_gui_window_preserving_mux(window: &Window) {
        window.close();
        front_end().forget_known_window(window);
    }

    fn close_gui_window_now(&self, window: &Window, preserve_mux_window: bool) {
        if !preserve_mux_window {
            // Local windows retain the established destructive close behavior.
            Mux::get().kill_window(self.mux_window_id);
        }
        Self::close_gui_window_preserving_mux(window);
    }

    fn close_requested(&mut self, window: &Window) {
        self.cancel_remote_divider_resizes_except(None);
        self.flush_right_sidebar_note_blocking();
        self.persist_workspace_layout_after_mutation("window close requested");

        let mux = Mux::get();
        let preserve_mux_window = self.should_preserve_mux_window_on_gui_close();
        match self.config.window_close_confirmation {
            WindowCloseConfirmation::NeverPrompt => {
                self.close_gui_window_now(window, preserve_mux_window);
            }
            WindowCloseConfirmation::AlwaysPrompt => {
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => {
                        self.close_gui_window_now(window, preserve_mux_window);
                        return;
                    }
                };

                let mux_window_id = self.mux_window_id;

                let can_close = mux
                    .get_window(mux_window_id)
                    .map_or(false, |w| w.can_close_without_prompting());
                if can_close {
                    self.close_gui_window_now(window, preserve_mux_window);
                    return;
                }
                let window = self.window.clone().unwrap();
                let (overlay, future) = start_overlay(self, &tab, move |tab_id, term| {
                    confirm_close_window(term, mux_window_id, window, tab_id, preserve_mux_window)
                });
                self.assign_overlay(tab.tab_id(), overlay);
                promise::spawn::spawn(future).detach();

                // Don't close right now; let the close happen from
                // the confirmation overlay
            }
        }
    }

    /// Save where the sidebar is scrolled for the Space currently adopted, so
    /// coming back to it lands where it was left rather than at the top.
    fn remember_workspace_sidebar_scroll(&mut self) {
        let offset = self.workspace_sidebar_scroll_offset;
        self.workspace_sidebar_scroll_offsets
            .insert(self.active_space_id.clone(), offset);
    }

    /// Where `active_space_id` was last scrolled to. A Space seen for the
    /// first time starts at the top. The value is not clamped here: the list
    /// it belongs to may have grown or shrunk since, and the paint clamps
    /// against the extent it actually measures.
    fn remembered_workspace_sidebar_scroll(&self) -> f32 {
        self.workspace_sidebar_scroll_offsets
            .get(&self.active_space_id)
            .copied()
            .unwrap_or(0.0)
    }

    /// The Space the left sidebar should render. Everything painting sidebar
    /// content must read this rather than `active_space_id`, or the offscreen
    /// paint of the neighbouring Space silently draws the adopted one instead
    /// and the swipe slides a page against its own copy.
    pub(crate) fn workspace_sidebar_space_id(&self) -> &str {
        self.workspace_sidebar_preview_space_id
            .as_deref()
            .unwrap_or(self.active_space_id.as_str())
    }

    /// The Space whose projects/domain the right-sidebar features (Files,
    /// remote connect, paste targets) should act on. A window displaying a
    /// thread reference shows a workspace that belongs to another Space, so
    /// resolve through the displayed workspace; everywhere else the window's
    /// Space is the answer.
    pub(crate) fn content_space_id(&self) -> String {
        if let Some(workspace) = self.current_mux_workspace() {
            // Only resolve through the displayed workspace when it is a
            // reference this Space actually holds. A freshly switched-to
            // window still shows the previous Space's terminal, and that
            // must not leak the previous machine into Files/paste.
            if let Some(space_id) = crate::workspace_threads::origin_space_for_ref_workspace(
                &self.active_space_id,
                &workspace,
            ) {
                return space_id;
            }
        }
        self.active_space_id.clone()
    }

    fn clear_workspace_space_swipe_frame_transition(&mut self) {
        self.workspace_space_swipe_source_frame = None;
        self.workspace_space_swipe_target_frame = None;
        self.workspace_space_swipe_tracked = false;
        self.workspace_sidebar_preview_space_id = None;
        self.workspace_space_swipe_capture_source = false;
        self.workspace_space_swipe_push_active = false;
        self.workspace_space_swipe_direction = 0.0;
        self.workspace_space_swipe_pending_commit = None;
        self.workspace_space_swipe_needs_settle_start = false;
    }

    /// Drop atlas-backed captures after the glyph atlas is recreated. A fast
    /// flick that committed before its first source paint has no stale source
    /// frame yet, so preserve its pending switch and retry that capture.
    fn recover_workspace_space_swipe_after_atlas_recreation(&mut self) {
        let preserve_pending =
            crate::termwindow::space_swipe::preserve_pending_source_capture_after_atlas_recreation(
                self.workspace_space_swipe_source_frame.is_some(),
                self.workspace_space_swipe_capture_source,
                self.workspace_space_swipe_pending_commit.is_some(),
            );
        if preserve_pending {
            self.workspace_space_swipe_target_frame = None;
            self.workspace_sidebar_preview_space_id = None;
            self.workspace_space_swipe_tracked = false;
            self.workspace_space_swipe_push_active = false;
            self.workspace_space_swipe_direction = 0.0;
            self.workspace_space_swipe_needs_settle_start = false;
        } else {
            self.workspace_sidebar_swipe.cancel_immediately();
            self.clear_workspace_space_swipe_frame_transition();
        }
    }

    /// Drop what a running full-window transition had recorded, keeping the
    /// transition itself.
    ///
    /// Same reason as the space swipe above: captured quads hold atlas UV
    /// coordinates rather than pixels, so a repack moves every glyph out from
    /// under them and replaying them afterwards draws whatever now sits at
    /// those coordinates. A transition is a likely moment for a repack -- it
    /// is generating thumbnails at font sizes nothing has used before.
    ///
    /// Most of it can simply be recorded again. Clearing `flight` is what asks
    /// for that: the next frame sees no recording, paints the terminal world
    /// into a fresh one, and re-resolves where it is going -- the timelines
    /// keep running throughout, so the animation continues from where it had
    /// reached rather than restarting. The window frame comes along on the
    /// same frame.
    ///
    /// `ghost` is kept, stale coordinates and all. It is a closing view's last
    /// frame and that view has already been torn down, so nothing can record
    /// it again -- and on the frame this runs on, the terminal world is going
    /// into a recording rather than onto the screen. Dropping the ghost there
    /// leaves nothing at all to draw: the whole window goes black for a frame,
    /// with the shrunken terminal alone in the middle of it. Wrong glyphs in a
    /// picture that is already fading out are much cheaper than that.
    ///
    /// This deliberately does *not* cancel the fade either. That was the first
    /// version, and it traded the same frame of wrong glyphs for an animation
    /// that silently stopped partway.
    fn discard_content_view_captures_after_atlas_recreation(&mut self) {
        // Repopulated by the paint pass the retry loop is about to run.
        self.content_view_last_frame = None;
        // Same reason, one level down: a card's recorded thumbnail is quads
        // carrying atlas coordinates, and a repack moves every glyph. Keeping
        // them would draw the overview in whatever now occupies those texels.
        // `PreviewQuadKey` also carries `shape_generation`, so this is belt and
        // braces -- but the two live in different files and the invariant
        // belongs with the rest of the captures.
        self.preview_quad_cache.borrow_mut().clear();
        // The half-built card holds the same kind of atlas-addressed quads.
        *self.preview_rebuild_partial.borrow_mut() = None;
        let Some(fade) = self.content_view_fade.as_mut() else {
            return;
        };
        // Logged, not silent. This fires far more often than "the atlas
        // occasionally repacks" suggests -- three times in thirteen seconds of
        // opening and closing the overview -- and every symptom it produces
        // looks like a rendering bug rather than like recovery.
        log::info!(
            "atlas recreated mid-transition; re-recording flight and chrome (ghost kept: {})",
            fade.ghost.is_some()
        );
        fade.flight = None;
        fade.chrome = None;
        // The card hides its own thumbnail while its terminal is in flight.
        // There is no flight for the moment, so let it draw one.
        if let Some(view) = self.active_content_view_mut() {
            view.set_terminal_in_flight(None);
        }
    }

    /// States that owe frames regardless of focus. The animation gate in
    /// paint and the unfocused repaint throttle must agree on this set,
    /// so both read it from here: a state added to one but not the other
    /// silently strands frames or throttles what must not be throttled.
    pub(crate) fn owes_frames_regardless_of_focus(&self) -> bool {
        self.content_view_fade.is_some()
            || self.active_content_view_index().is_some()
            || self.workspace_sidebar_hover.needs_frames()
            || self.overlay_scrollbar_owes_frames()
    }

    /// An overlay scrollbar that is still up has a fade to finish; the
    /// wheel scrolls an unfocused window too, and a thumb left standing
    /// until some output happens to repaint would be a visible glitch.
    fn overlay_scrollbar_owes_frames(&self) -> bool {
        let now = Instant::now();
        self.pane_state.borrow().values().any(|state| {
            state
                .scrollbar_visible_until
                .is_some_and(|until| until > now)
                || state.scrollbar_expand_tick.is_some()
        })
    }

    /// Whether the pane's overlay scrollbar should be at its hovered
    /// thickness: the pointer is on its strip, or its thumb is being
    /// dragged.
    pub(crate) fn overlay_scrollbar_hovered(&self, pane_id: PaneId) -> bool {
        let on_track = |item: &UIItem| {
            matches!(
                item.item_type,
                UIItemType::ScrollThumb(track)
                    | UIItemType::AboveScrollThumb(track)
                    | UIItemType::BelowScrollThumb(track)
                    if track.pane_id == pane_id
            )
        };
        self.dragging.as_ref().is_some_and(|(item, _)| on_track(item))
            || (self.current_mouse_event.is_some()
                && self.last_ui_item.as_ref().is_some_and(on_track))
    }

    /// Advance the pane's overlay scrollbar towards its hovered or resting
    /// thickness and return the current factor, 0 to 1. Says when the next
    /// frame is due while the change is still in flight.
    pub(crate) fn overlay_scrollbar_expand(
        &self,
        pane_id: PaneId,
        hovered: bool,
        now: Instant,
    ) -> (f32, Option<Instant>) {
        let mut state = self.pane_state(pane_id);
        let target = if hovered { 1.0 } else { 0.0 };
        if state.scrollbar_expand == target {
            state.scrollbar_expand_tick = None;
            return (target, None);
        }
        let step = match state.scrollbar_expand_tick {
            Some(tick) => {
                now.duration_since(tick).as_secs_f32() / OVERLAY_SCROLLBAR_EXPAND.as_secs_f32()
            }
            None => 0.0,
        };
        let expand = if target > state.scrollbar_expand {
            (state.scrollbar_expand + step).min(1.0)
        } else {
            (state.scrollbar_expand - step).max(0.0)
        };
        state.scrollbar_expand = expand;
        if expand == target {
            state.scrollbar_expand_tick = None;
            (expand, None)
        } else {
            state.scrollbar_expand_tick = Some(now);
            (expand, Some(now + Duration::from_millis(16)))
        }
    }

    /// Tracks whether the user can see this window at all. macOS reports
    /// the transition via WindowEvent::OcclusionChanged; other platforms
    /// never emit it and the window simply counts as always visible.
    fn occlusion_changed(&mut self, visible: bool, _window: &Window) {
        log::trace!("Setting occlusion visible={visible:?}");
        if visible {
            self.occluded = None;
        } else if self.occluded.is_none() {
            self.occluded = Some(Instant::now());
            self.occlusion_released = false;
        }
    }

    /// A window nobody can see keeps everything it ever cached. Once it
    /// has stayed hidden past the grace period, drop the caches that
    /// rebuild lazily on the reveal repaint: the note and file-preview
    /// shape caches (48MiB of budget), the vertex buffers (an overview
    /// spike pins tens of megabytes), and the line quad cache. The glyph
    /// atlas is deliberately left alone — rebuilding it re-rasterizes
    /// every glyph on the reveal frame.
    ///
    /// Driven from the 1s status heartbeat rather than a one-shot timer:
    /// display sleep can swallow a detached timer whole (the throttle
    /// latch documents the same failure), while an elapsed() check
    /// self-heals, and it releases again if a hidden repaint (a resize
    /// completing, a config reload) regrew what an earlier release
    /// dropped.
    fn maybe_release_occluded_memory(&mut self) {
        if !occlusion_release_due(
            self.occluded.map(|since| since.elapsed()),
            self.occlusion_released,
        ) {
            return;
        }
        // Damping: even when hidden repaints keep re-marking the episode
        // dirty, the release runs at most once per grace period.
        if self
            .occlusion_last_release
            .is_some_and(|t| t.elapsed() < Duration::from_secs(OCCLUSION_RELEASE_SECS))
        {
            return;
        }
        self.occlusion_released = true;
        self.occlusion_last_release = Some(Instant::now());
        log::debug!("window occluded for {OCCLUSION_RELEASE_SECS}s; releasing caches");
        {
            let mut caches = self.ui_shape_caches.borrow_mut();
            caches.clear_note();
            caches.clear_file_preview();
        }
        self.publish_ui_shape_cache_diagnostics();
        if let Some(render_state) = self.render_state.as_ref() {
            render_state.shrink_quads_now();
            // A hidden window paints no frames, so these could not age out
            // through end_frame; re-upload on the next paint is cheap.
            render_state.dedicated_images.borrow_mut().clear();
        }
        self.line_quad_cache.borrow_mut().clear();
        // Likewise the scratch recorder: a hidden window records no lines.
        *self.line_quad_scratch.borrow_mut() = HeapQuadAllocator::default();
        // The buffers just dropped stay resident until a device maintain
        // runs, and an occluded window submits no frames — poll once so
        // the memory actually returns now rather than at the reveal.
        if let Some(webgpu) = self.webgpu.as_ref() {
            if let Err(err) = webgpu.device.poll(wgpu::PollType::Poll) {
                log::debug!("device poll after occlusion release: {err:?}");
            }
        }
        // Set the sticky invalidated bit so the occlusion re-arm repaints
        // us on reveal instead of presenting a stale frame.
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    fn focus_changed(&mut self, focused: bool, window: &Window) {
        if focused == self.focused.is_some() {
            // Level rechecks (and AppKit itself) can repeat an edge; a
            // duplicate FocusChanged(true) would restart the
            // click-swallow window, flush the quad caches and re-fire
            // user-visible focus events for no actual change.
            return;
        }
        log::trace!("Setting focus to {:?}", focused);
        self.focused = if focused { Some(Instant::now()) } else { None };
        if focused {
            // Focus implies visible; belt-and-braces cover for a lost
            // occlusion edge (the window backend re-posts rather than
            // drops them). If a window somehow gains key focus while
            // genuinely still hidden, this skips at most one episode's
            // release — accepted.
            self.occluded = None;
        }
        // Disarm the unfocused repaint throttle either way: on focus the
        // invalidate below repaints immediately and a stale latch would
        // swallow the next output event; on blur the first output should
        // paint promptly (leading edge) before the throttle kicks in.
        self.unfocused_invalidate_due = None;
        self.unfocused_next_allowed = Instant::now();
        // An unfocused window gets by with one less swapchain drawable
        // (one framebuffer-sized allocation); applied lazily at the next
        // paint's reconfigure. Guarded: FocusChanged can arrive before
        // the async webgpu setup finishes.
        if let Some(webgpu) = self.webgpu.as_ref() {
            webgpu.set_desired_frame_latency(if focused { 2 } else { 1 });
        }
        self.quad_generation += 1;
        self.load_os_parameters();

        if focused {
            // Each window is pinned to its own Space (workspace). Keep the
            // mux's single global "active workspace" pointed at whichever
            // window is focused, so defaults like spawn-new-window land in the
            // right place. Reconcile is additive, so this is non-destructive.
            let mux = Mux::get();
            let ws = mux
                .get_window(self.mux_window_id)
                .map(|w| w.get_workspace().to_string());
            if let Some(ws) = ws {
                if mux.active_workspace() != ws {
                    mux.set_active_workspace(&ws);
                }
            }
        }

        if self.focused.is_none() {
            self.workspace_sidebar_swipe.cancel_immediately();
            self.workspace_sidebar_hover.cancel_immediately();
            self.clear_workspace_space_swipe_frame_transition();
            self.right_sidebar_note.native_text_input_snapshot_key = None;
            window.set_native_text_input_snapshot(None);
            self.right_sidebar_note.freeze_live_source();
            self.save_right_sidebar_note_now();
            self.last_mouse_click = None;
            self.current_mouse_buttons.clear();
            self.current_mouse_capture = None;
            self.is_click_to_focus_window = false;

            for state in self.pane_state.borrow_mut().values_mut() {
                state.mouse_terminal_coords.take();
            }

            // Losing window focus dismisses the command palette, commits a
            // pending inline rename (same as clicking away) and abandons any
            // in-flight file drag.
            self.close_command_palette();
            self.finish_inline_tab_rename(true);
            self.right_sidebar_file_drag = None;
            self.pane_tab_drag = None;
            self.sidebar_row_drag = None;
            let lost_split_drag = self
                .dragging
                .as_ref()
                .is_some_and(|(item, _)| matches!(item.item_type, UIItemType::Split(_)));
            if lost_split_drag {
                self.finish_remote_split_drag();
                self.persist_workspace_layout_after_mutation("split drag lost focus");
            }
            self.dragging = None;
        }

        // Reset the cursor blink phase
        self.prev_cursor.bump();

        // force cursor to be repainted
        window.invalidate();

        if let Some(pane) = self.get_active_pane_or_overlay() {
            pane.focus_changed(focused);
        }

        self.update_title();
        self.emit_window_event("window-focus-changed", None);

        // On regaining focus, refresh the Files tree + (re)start its periodic
        // re-scan; on blur the cycle lapses (its next tick gates on focus).
        if focused {
            self.kick_right_sidebar_file_rescan_cycle();
        }
    }

    fn created(&mut self, ctx: RenderContext) -> anyhow::Result<()> {
        self.render_state = None;

        let render_info = ctx.renderer_info();
        self.opengl_info.replace(render_info.clone());

        match RenderState::new(ctx, &self.fonts, &self.render_metrics, ATLAS_SIZE) {
            Ok(render_state) => {
                log::debug!(
                    "OpenGL initialized! {} wezterm version: {}",
                    render_info,
                    config::wezterm_version(),
                );
                self.render_state.replace(render_state);
            }
            Err(err) => {
                log::error!("failed to create RenderState: {}", err);
            }
        }

        if self.render_state.is_none() {
            panic!("No OpenGL");
        }

        Ok(())
    }
}

impl TermWindow {
    pub async fn new_window(mux_window_id: MuxWindowId) -> anyhow::Result<()> {
        Self::new_window_impl(mux_window_id, None, true).await
    }

    /// For domain-owned mux windows (remote mux windows arriving via a
    /// ClientDomain, tmux): create the GUI window as-is, without restoring
    /// a saved workspace thread. Restoring would adopt this window onto a
    /// different mux window, orphaning/killing the one the domain created.
    pub async fn new_window_without_restore(mux_window_id: MuxWindowId) -> anyhow::Result<()> {
        Self::new_window_impl(mux_window_id, None, false).await
    }

    pub async fn new_window_with_claimed_space(
        mux_window_id: MuxWindowId,
        space_owner_id: u64,
        active_space_id: String,
    ) -> anyhow::Result<()> {
        Self::new_window_impl(
            mux_window_id,
            Some((space_owner_id, active_space_id)),
            false,
        )
        .await
    }

    async fn new_window_impl(
        mux_window_id: MuxWindowId,
        claimed_space: Option<(u64, String)>,
        restore_saved_thread: bool,
    ) -> anyhow::Result<()> {
        let config = configuration();
        let native_settings = crate::native_settings::load();
        // A palette-picked color scheme applies from the first frame; seeding
        // config_overrides here is what makes it stick for new windows.
        let (config, config_overrides) = match crate::native_settings::effective_color_scheme(
            &native_settings,
            &config,
        ) {
            Some(scheme) => {
                use wezterm_dynamic::ToDynamic;
                // Deliberately louder than trace: this silently overrides
                // `color_scheme` from the config file, and "why doesn't my
                // config change do anything" needs a breadcrumb.
                log::info!(
                    "color scheme overridden to {scheme:?} -- either picked in the command \
                     palette or in Settings, or the light default that goes with a light \
                     interface; pick \"Use configured default\" in the palette to follow the \
                     config file again"
                );
                let mut obj = wezterm_dynamic::Object::default();
                obj.insert("color_scheme".to_dynamic(), scheme.to_dynamic());
                let overrides = wezterm_dynamic::Value::Object(obj);
                match config::overridden_config(&overrides) {
                    Ok(config) => (config, overrides),
                    Err(err) => {
                        log::warn!("failed to apply saved color scheme: {err:#}");
                        (config, wezterm_dynamic::Value::default())
                    }
                }
            }
            None => (config, wezterm_dynamic::Value::default()),
        };
        let main_renderer =
            crate::native_settings::main_window_renderer(&native_settings, config.front_end);
        let dpi = config.dpi.unwrap_or_else(|| ::window::default_dpi()) as usize;
        let fontconfig = Rc::new(FontConfiguration::new(Some(config.clone()), dpi)?);

        let mux = Mux::get();
        let size = match mux.get_active_tab_for_window(mux_window_id) {
            Some(tab) => tab.get_size(),
            None => {
                // Window created ahead of its content (e.g. the connect flow
                // creates the window before the domain attach populates it).
                // A zero Default here would open a degenerate one-row window
                // and set off a resize tug-of-war; start at the configured
                // initial size instead.
                log::debug!("new_window has no tabs... yet?");
                let dpi = config.dpi.unwrap_or_else(|| ::window::default_dpi());
                config.initial_size(dpi as u32, crate::cell_pixel_dims(&config, dpi as f64).ok())
            }
        };
        let physical_rows = size.rows as usize;
        let physical_cols = size.cols as usize;

        let render_metrics = RenderMetrics::new(&fontconfig)?;
        log::trace!("using render_metrics {:#?}", render_metrics);

        // Initially we have only a single tab, so take that into account
        // for the tab bar state.
        let show_tab_bar = config.enable_tab_bar && !config.hide_tab_bar_if_only_one_tab;
        let tab_bar_height = if show_tab_bar {
            Self::tab_bar_pixel_height_impl(&config, &fontconfig, &render_metrics)? as usize
        } else {
            0
        };

        let terminal_size = TerminalSize {
            rows: physical_rows,
            cols: physical_cols,
            pixel_width: (render_metrics.cell_size.width as usize * physical_cols),
            pixel_height: (render_metrics.cell_size.height as usize * physical_rows),
            dpi: dpi as u32,
        };

        if terminal_size != size {
            // DPI is different from the default assumed DPI when the mux
            // created the pty. We need to inform the kernel of the revised
            // pixel geometry now
            log::trace!(
                "Initial geometry was {:?} but dpi-adjusted geometry \
                        is {:?}; update the kernel pixel geometry for the ptys!",
                size,
                terminal_size,
            );
            if let Some(window) = mux.get_window(mux_window_id) {
                for tab in window.iter() {
                    tab.resize(terminal_size);
                }
            };
        }

        let h_context = DimensionContext {
            dpi: dpi as f32,
            pixel_max: terminal_size.pixel_width as f32,
            pixel_cell: render_metrics.cell_size.width as f32,
        };
        let padding_left = config.window_padding.left.evaluate_as_pixels(h_context) as usize;
        let workspace_sidebar_width =
            ui::workspace_sidebar_width_for_metrics(&render_metrics, dpi as usize);
        let padding_right = resize::effective_right_padding(&config, h_context) as usize;
        let v_context = DimensionContext {
            dpi: dpi as f32,
            pixel_max: terminal_size.pixel_height as f32,
            pixel_cell: render_metrics.cell_size.height as f32,
        };
        let padding_top = config.window_padding.top.evaluate_as_pixels(v_context) as usize;
        let padding_bottom = config.window_padding.bottom.evaluate_as_pixels(v_context) as usize;

        let mut dimensions = Dimensions {
            pixel_width: (terminal_size.pixel_width
                + workspace_sidebar_width
                + padding_left
                + padding_right) as usize,
            pixel_height: ((terminal_size.rows * render_metrics.cell_size.height as usize)
                + padding_top
                + padding_bottom) as usize
                + tab_bar_height,
            dpi,
        };

        let border = Self::get_os_border_impl(&None, &config, &dimensions, &render_metrics);

        dimensions.pixel_height += (border.top + border.bottom).get() as usize;
        dimensions.pixel_width += (border.left + border.right).get() as usize;

        let window_background = load_background_image(&config, &dimensions, &render_metrics);

        log::trace!(
            "TermWindow::new_window called with mux_window_id {} {:?} {:?}",
            mux_window_id,
            terminal_size,
            dimensions
        );

        let render_state = None;

        let connection_name = Connection::get().unwrap().name();
        let dies_with_mux_window = claimed_space.is_none() && !restore_saved_thread;
        let (space_owner_id, active_space_id) = claimed_space.unwrap_or_else(|| {
            let space_owner_id = crate::workspace_threads::next_space_owner_id();
            // Windows created without a saved-thread restore are incidental
            // (domain-owned windows spawned by the reconcile, e.g. reconnect
            // or auth prompt windows); claiming a Space for them must not
            // overwrite the user's last-active-Space record.
            let active_space_id = if restore_saved_thread {
                crate::workspace_threads::claim_initial_space_for_window(space_owner_id)
            } else {
                crate::workspace_threads::claim_space_for_incidental_window(space_owner_id)
            };
            (space_owner_id, active_space_id)
        });
        let workspace_layout_structure_fingerprint =
            crate::workspace_threads::window_layout_structure_fingerprint(mux_window_id);
        let active_space_has_note_vault =
            crate::workspace_threads::space_note_vault(&active_space_id).is_some();

        let myself = Self {
            frontend_geometry_phases: HashMap::new(),
            frontend_geometry_confirmations: HashMap::new(),
            frontend_recovery_geometry: HashMap::new(),
            frontend_geometry_resync_after_epoch: HashSet::new(),
            rejected_local_viewports: HashMap::new(),
            rejected_client_viewports: HashMap::new(),
            client_viewport_publish_seq: HashMap::new(),
            right_sidebar_truncated_file_rows: HashSet::new(),
            right_sidebar_truncated_remote_file_rows: HashSet::new(),
            remote_divider_resize_streams: HashMap::new(),
            next_frontend_geometry_epoch: 1,
            frontend_viewport_report_pending: Arc::new(AtomicBool::new(false)),
            frontend_handoff_consumed_press: false,
            created: Instant::now(),
            connection_name,
            last_fps_check_time: Instant::now(),
            num_frames: 0,
            last_frame_duration: Duration::ZERO,
            fps: 0.,
            config_subscription: None,
            os_parameters: None,
            gl: None,
            webgpu: None,
            window: None,
            window_background,
            config: config.clone(),
            config_overrides,
            palette: None,
            focused: None,
            occluded: None,
            occlusion_released: false,
            occlusion_last_release: None,
            mux_window_id,
            mux_window_id_for_subscriptions: Arc::new(Mutex::new(mux_window_id)),
            pane_subscription_dead: RefCell::new(None),
            watchdog_forced_repaints: 0,
            watchdog_presented_seen: HashMap::new(),
            watchdog_last_forced: None,
            fonts: Rc::clone(&fontconfig),
            render_metrics,
            dimensions,
            window_state: WindowState::default(),
            resizes_pending: 0,
            is_repaint_pending: false,
            pending_scale_changes: LinkedList::new(),
            terminal_size,
            render_state,
            input_map: InputMap::new(&config),
            leader_is_down: None,
            dead_key_status: DeadKeyStatus::None,
            show_tab_bar,
            show_scroll_bar: config.enable_scroll_bar,
            tab_bar: TabBarState::default(),
            fancy_tab_bar: None,
            tab_bar_scroll_offset: 0.0,
            tab_bar_scroll_target: 0.0,
            pane_nav_tab_scroll_offsets: HashMap::new(),
            pane_nav_tab_scroll_targets: HashMap::new(),
            collapsed_pane_layouts: HashMap::new(),
            tab_wheel_surface_lock: None,
            tab_wheel_direction_lock: None,
            inline_tab_rename: None,
            pane_tab_title_overrides: HashMap::new(),
            right_status: String::new(),
            left_status: String::new(),
            last_mouse_coords: (0, -1),
            window_drag_position: None,
            current_mouse_event: None,
            current_modifier_and_leds: Default::default(),
            prev_cursor: PrevCursorPos::new(),
            last_scroll_info: RenderableDimensions::default(),
            tab_state: RefCell::new(HashMap::new()),
            pane_state: RefCell::new(HashMap::new()),
            frame_pane_output_generations: HashMap::new(),
            track_pane_output_generations_for_frame: false,
            pane_font_cache: RefCell::new(HashMap::new()),
            pane_font_cache_tick: Cell::new(0),
            preview_quad_cache: RefCell::new(HashMap::new()),
            preview_rebuild_partial: RefCell::new(None),
            preview_content_fade: RefCell::new(HashMap::new()),
            pending_card_renders: RefCell::new(Vec::new()),
            card_frame_verts: RefCell::new(Vec::new()),
            card_composites: RefCell::new(Vec::new()),
            image_composites: RefCell::new(Default::default()),
            dedicated_image_in_line: std::cell::Cell::new(false),
            line_render_y_offset: std::cell::Cell::new(0.0),
            dedicated_image_cell: std::cell::Cell::new(None),
            content_view_last_composites: RefCell::new(Vec::new()),
            card_scratch: RefCell::new(None),
            preview_scale_hold: RefCell::new(HashMap::new()),
            current_mouse_buttons: vec![],
            current_mouse_capture: None,
            last_mouse_click: None,
            current_highlight: None,
            quad_generation: 0,
            chrome_palette: crate::native_settings::chrome_palette(
                crate::native_settings::load_shared().appearance.theme_mode,
                crate::native_settings::effective_appearance(),
                &config,
                None,
            ),
            shape_generation: 0,
            shape_cache: RefCell::new(LfuCache::new_weighted(
                "shape_cache.hit.rate",
                "shape_cache.miss.rate",
                |config| config.shape_cache_size,
                shape_cache_budget,
                &config,
            )),
            ui_shape_caches: RefCell::new(crate::shapecache::UiShapeCaches::new(&config)),
            ui_text_domain: std::cell::Cell::new(crate::shapecache::UiTextDomain::Chrome),
            last_ui_shape_diagnostics_publish: std::cell::Cell::new(None),
            line_state_cache: RefCell::new(LfuCacheU64::new(
                "line_state_cache.hit.rate",
                "line_state_cache.miss.rate",
                |config| config.line_state_cache_size,
                &config,
            )),
            next_line_state_id: 0,
            line_quad_cache: RefCell::new(LfuCache::new_weighted(
                "line_quad_cache.hit.rate",
                "line_quad_cache.miss.rate",
                |config| config.line_quad_cache_size,
                line_quad_cache_budget,
                &config,
            )),
            line_quad_scratch: RefCell::new(HeapQuadAllocator::default()),
            line_to_ele_shape_cache: RefCell::new(LfuCache::new_weighted(
                "line_to_ele_shape_cache.hit.rate",
                "line_to_ele_shape_cache.miss.rate",
                |config| config.line_to_ele_shape_cache_size,
                line_to_ele_shape_cache_budget,
                &config,
            )),
            last_status_call: Instant::now(),
            last_gpu_allocator_log: Cell::new(None),
            cursor_blink_state: RefCell::new(ColorEase::new(
                config.cursor_blink_rate,
                config.cursor_blink_ease_in,
                config.cursor_blink_rate,
                config.cursor_blink_ease_out,
                None,
            )),
            blink_state: RefCell::new(ColorEase::new(
                config.text_blink_rate,
                config.text_blink_ease_in,
                config.text_blink_rate,
                config.text_blink_ease_out,
                None,
            )),
            rapid_blink_state: RefCell::new(ColorEase::new(
                config.text_blink_rate_rapid,
                config.text_blink_rapid_ease_in,
                config.text_blink_rate_rapid,
                config.text_blink_rapid_ease_out,
                None,
            )),
            event_states: HashMap::new(),
            current_event: None,
            has_animation: RefCell::new(None),
            scheduled_animation: RefCell::new(None),
            unfocused_invalidate_due: None,
            unfocused_next_allowed: Instant::now(),
            allow_images: AllowImage::Yes,
            atlas_scale_hold: None,
            atlas_overflow_log: std::collections::VecDeque::new(),
            semantic_zones: HashMap::new(),
            ui_items: vec![],
            context_menu: None,
            command_palette: None,
            text_min_contrast: crate::native_settings::text_min_contrast_ratio(&config),
            min_contrast_memo: RefCell::new(HashMap::new()),
            scheme_preview: None,
            scheme_preview_ground: None,
            context_menu_application_actions: HashMap::new(),
            next_context_menu_application_action_id: 1,
            context_menu_suppressed_release: None,
            dragging: None,
            right_sidebar_file_drag: None,
            pane_tab_drag: None,
            sidebar_row_drag: None,
            content_views: vec![],
            active_content_view_id: None,
            content_view_response_tab_id: None,
            content_view_deferred_mux_resize: false,
            content_view_fade: None,
            content_view_last_frame: None,
            next_content_view_id: 1,
            registered_content_view_surfaces: HashMap::new(),
            remote_connects: HashMap::new(),
            next_remote_connect_generation: 1,
            mosh_connects: HashMap::new(),
            next_mosh_connect_generation: 1,
            local_thread_activation: None,
            next_local_thread_activation_generation: 1,
            space_owner_id,
            active_space_id,
            dies_with_mux_window,
            space_reconnects_in_flight: HashSet::new(),
            workspace_layout_structure_fingerprint,
            workspace_sidebar_width,
            workspace_sidebar_pending_thread_selection: None,
            workspace_sidebar_collapsed: !native_settings.onboarding.show_left_sidebar_by_default,
            workspace_sidebar_scroll_offset: 0.0,
            workspace_sidebar_scroll_offsets: HashMap::new(),
            workspace_sidebar_swipe: space_swipe::SidebarSpaceSwipeState::default(),
            workspace_space_swipe_source_frame: None,
            workspace_space_swipe_capture_source: false,
            workspace_sidebar_preview_space_id: None,
            workspace_space_swipe_target_frame: None,
            workspace_space_swipe_tracked: false,
            workspace_sidebar_list_quads: None,
            workspace_space_swipe_push_active: false,
            workspace_space_swipe_direction: 0.0,
            workspace_space_swipe_pending_commit: None,
            workspace_space_swipe_needs_settle_start: false,
            workspace_sidebar_scrollbar_visible_until: None,
            workspace_sidebar_hover: sidebar_hover::SidebarHoverReveal::default(),
            titlebar_sidebar_button_hovered: false,
            thread_ref_groups_collapsed: std::collections::HashSet::new(),
            workspace_sidebar_show_archived: false,
            native_context_menu_open: false,
            workspace_notification_snapshot: None,
            workspace_notification_pulse_started_at: None,
            right_sidebar_width: ui::right_sidebar_width_for_metrics(&render_metrics, dpi as usize),
            right_sidebar_collapsed: true,
            right_sidebar_mode: RightSidebarMode::Snippets,
            right_sidebar_agents_scroll: 0.0,
            right_sidebar_snippet_view: RightSidebarSnippetView::List,
            right_sidebar_snippet_focus: None,
            right_sidebar_snippet_search: TextInputState::new(),
            right_sidebar_snippet_title: TextInputState::new(),
            right_sidebar_snippet_body: TextInputState::new(),
            right_sidebar_snippet_scroll_offset: 0.0,
            right_sidebar_snippet_scrollbar_visible_until: None,
            right_sidebar_note: crate::markdown_editor::NoteHostState::default(),
            right_sidebar_note_view: RightSidebarNoteView::Editor,
            right_sidebar_note_vault_index_root: None,
            right_sidebar_note_vault_paths: Arc::new(Vec::new()),
            right_sidebar_note_vault_index_generation: 0,
            right_sidebar_note_vault_indexing: false,
            right_sidebar_note_vault_last_scan: None,
            right_sidebar_note_open_generation: 0,
            right_sidebar_note_opening: None,
            right_sidebar_note_open_failure: None,
            right_sidebar_note_vault_failure: None,
            right_sidebar_note_tree_scroll_offset: 0.0,
            right_sidebar_note_tree_expanded: HashSet::new(),
            right_sidebar_note_vault_tree_collapsed: false,
            right_sidebar_note_wide_layout: false,
            right_sidebar_note_table_horizontal_offsets: std::collections::BTreeMap::new(),
            right_sidebar_note_table_layouts: Vec::new(),
            right_sidebar_note_images: HashMap::new(),
            right_sidebar_note_images_loading: HashSet::new(),
            right_sidebar_note_image_order: VecDeque::new(),
            right_sidebar_note_image_failures: HashMap::new(),
            right_sidebar_note_code_highlight: ui::right_sidebar::NoteCodeHighlightState::default(),
            right_sidebar_note_paint_cache: ui::right_sidebar::NotePaintCache::default(),
            right_sidebar_note_prewarm: None,
            right_sidebar_note_memory_release_token: 0,
            right_sidebar_file_view: RightSidebarFileView::Tree,
            right_sidebar_file_focus: None,
            right_sidebar_file_filter: TextInputState::new(),
            right_sidebar_file_applied_filter: String::new(),
            right_sidebar_file_filter_debounce_until: None,
            right_sidebar_file_expanded: HashSet::new(),
            right_sidebar_file_expanded_version: 0,
            right_sidebar_file_index_generation: 0,
            right_sidebar_file_index_root: None,
            right_sidebar_file_index_project_name: String::new(),
            right_sidebar_file_index_status: RightSidebarFileIndexStatus::Empty,
            right_sidebar_file_memory_release_token: 0,
            right_sidebar_file_index: None,
            right_sidebar_file_index_cancel: None,
            right_sidebar_file_dir_cache: RightSidebarFileDirCache::default(),
            right_sidebar_file_dir_loads_in_flight: HashSet::new(),
            right_sidebar_file_dir_cache_generation: 0,
            right_sidebar_file_view_needs_restore: false,
            right_sidebar_file_search_generation: 0,
            right_sidebar_file_search_cancel: None,
            right_sidebar_file_search_query: String::new(),
            right_sidebar_file_search_rows: Vec::new(),
            right_sidebar_file_searching: false,
            right_sidebar_file_browse_rows: Vec::new(),
            right_sidebar_file_browse_cache_key: None,
            right_sidebar_file_selected: None,
            right_sidebar_file_tree_width: ui::right_sidebar_width_for_metrics(
                &render_metrics,
                dpi as usize,
            ),
            right_sidebar_file_preview_width: ui::right_sidebar_file_preview_width(dpi as usize),
            right_sidebar_note_pane_expanded:
                crate::native_settings::right_sidebar_note_pane_expanded(),
            right_sidebar_note_pane_width: ui::right_sidebar_note_pane_width_for_dpi(dpi as usize),
            active_space_has_note_vault,
            pending_sidebar_reflow_width: None,
            right_sidebar_file_preview_generation: 0,
            right_sidebar_file_preview_highlight_cancel: Arc::new(AtomicUsize::new(0)),
            right_sidebar_file_preview_lines: Vec::new(),
            right_sidebar_file_preview_raw_text: None,
            right_sidebar_file_preview_max_columns: 0,
            right_sidebar_file_preview_image: None,
            right_sidebar_file_preview_message: None,
            right_sidebar_file_preview_truncated: false,
            right_sidebar_file_preview_selection: None,
            right_sidebar_file_preview_slice_cache: RefCell::new(HashMap::new()),
            right_sidebar_file_preview_slice_cache_order: RefCell::new(VecDeque::new()),
            right_sidebar_file_preview_line_color_cache: RefCell::new(HashMap::new()),
            right_sidebar_file_preview_line_color_cache_order: RefCell::new(VecDeque::new()),
            right_sidebar_file_tree_scroll_offset: 0.0,
            right_sidebar_file_preview_scroll_offset: 0.0,
            right_sidebar_file_preview_horizontal_offset: 0,
            right_sidebar_file_preview_restore_scroll: None,
            right_sidebar_file_view_state_by_root: HashMap::new(),
            right_sidebar_file_view_state_order: VecDeque::new(),
            right_sidebar_remote_files: remote_files::RemoteFilesState::default(),
            right_sidebar_remote_files_lease: None,
            right_sidebar_remote_transfers: Vec::new(),
            right_sidebar_remote_transfer_next_id: 0,
            right_sidebar_remote_drop_target: None,
            right_sidebar_local_drop_target: None,
            pending_local_copy: None,
            pending_remote_confirm: None,
            pending_remote_rename: None,
            local_copy_generation: 0,
            pending_local_copy_conflict_count: 0,
            right_sidebar_remote_file_tree_scroll_offset: 0.0,
            right_sidebar_file_rescan_token: 0,
            right_sidebar_file_refreshing: false,
            right_sidebar_open_with_generation: 0,
            right_sidebar_open_with_cache: HashMap::new(),
            right_sidebar_open_with_app: crate::native_settings::right_sidebar_open_with_app(),
            right_sidebar_input_layouts: Vec::new(),
            last_ui_item: None,
            hover_tooltip: None,
            is_click_to_focus_window: false,
            key_table_state: KeyTableState::default(),
            modal: RefCell::new(None),
            opengl_info: None,
            restored_frame_target: None,
            restored_frame_attempts: 0,
            restored_frame_deadline: None,
        };

        let tw = Rc::new(RefCell::new(myself));
        let tw_event = Rc::clone(&tw);

        let mut x = None;
        let mut y = None;
        let mut origin = GeometryOrigin::default();

        if let Some(position) = mux
            .get_window(mux_window_id)
            .and_then(|window| window.get_initial_position().clone())
            .or_else(|| POSITION.lock().unwrap().take())
        {
            x.replace(position.x);
            y.replace(position.y);
            origin = position.origin;
        }

        // The main window -- the first one this process opens -- reopens where
        // it was last left, provided that is still somewhere on this desktop.
        // `claim_main_window` runs first and unconditionally: a window that is
        // not restoring its frame is still the window that records one, should
        // the setting be turned on while it is open.
        //
        // An explicit position outranks a remembered one. Someone who passed
        // `--position`, or moved the window from the CLI, is asking about this
        // launch, not the last one.
        //
        // macOS is untouched by any of this and keeps using AppKit's frame
        // autosave below; `frame_to_restore` yields nothing there.
        let restored_frame = if crate::main_window_placement::claim_main_window(mux_window_id)
            && native_settings.window.restore_main_window_frame
            && x.is_none()
            && y.is_none()
        {
            crate::main_window_placement::frame_to_restore()
        } else {
            None
        };

        let geometry = RequestedWindowGeometry {
            width: Dimension::Pixels(dimensions.pixel_width as f32),
            height: Dimension::Pixels(dimensions.pixel_height as f32),
            x,
            y,
            macos_frame_autosave_name: if cfg!(target_os = "macos")
                && native_settings.window.restore_main_window_frame
            {
                Some("ThinkTerm.MainWindow".to_string())
            } else {
                None
            },
            windows_frame_rect: restored_frame.map(|restored| restored.frame),
            origin,
        };
        log::trace!("{:?}", geometry);

        let window = Window::new_window(
            &get_window_class(),
            "ThinkTerm",
            geometry,
            Some(&config),
            Rc::clone(&fontconfig),
            move |event, window| {
                let mut tw = tw_event.borrow_mut();
                if let Err(err) = tw.dispatch_window_event(event, window) {
                    log::error!("dispatch_window_event: {:#}", err);
                }
            },
        )
        .await?;
        window.set_titlebar_sidebar_button_visible(true);
        {
            let mut tw = tw.borrow_mut();
            tw.window.replace(window.clone());
            // Opening at the remembered rect is only half of it; see
            // `settle_restored_frame` for what startup does to it afterwards.
            if let Some(restored) = restored_frame.filter(|restored| !restored.maximized) {
                tw.restored_frame_target = Some(restored.frame);
                tw.restored_frame_attempts = RESTORED_FRAME_ATTEMPTS;
                tw.restored_frame_deadline = Some(Instant::now() + RESTORED_FRAME_SETTLE_TIME);
            }
        }

        Self::apply_icon(&window)?;

        // Content attached before this window existed -- the local session
        // server's panes on startup -- was configured from the global
        // configuration; give it this window's, colour scheme included.
        window.notify(TermWindowNotif::Apply(Box::new(|tw| {
            tw.push_window_config_to_all_tabs();
        })));

        let config_subscription = config::subscribe_to_config_reload({
            let window = window.clone();
            move || {
                window.notify(TermWindowNotif::Apply(Box::new(|tw| {
                    tw.config_was_reloaded()
                })));
                true
            }
        });

        // Try WebGpu first when it is the selection, but never let its failure
        // be fatal: a machine with no usable Vulkan/DX12/Metal adapter would
        // otherwise get a window that never opens *and* no way back, because
        // the settings window is built the same way and would fail with it.
        // Falling back costs a log line; not falling back costs the app.
        let mut effective_renderer = main_renderer;
        let mut webgpu = None;
        if matches!(
            main_renderer,
            crate::native_settings::NativeRendererBackend::WebGpu
        ) {
            gpu_debug(format!(
                "create WebGpu main_window size={}x{} dpi={}",
                dimensions.pixel_width, dimensions.pixel_height, dimensions.dpi
            ));
            match WebGpuState::new(&window, dimensions, &config).await {
                Ok(state) => webgpu = Some(Rc::new(state)),
                Err(err) => {
                    log::error!("WebGpu is unavailable ({err:#}); falling back to OpenGL");
                    gpu_debug(format!(
                        "WebGpu unavailable: {err:#}; falling back to OpenGL"
                    ));
                    effective_renderer = crate::native_settings::NativeRendererBackend::OpenGL;
                }
            }
        }

        let gl = match effective_renderer {
            crate::native_settings::NativeRendererBackend::WebGpu => None,
            crate::native_settings::NativeRendererBackend::OpenGL => {
                gpu_debug(format!(
                    "enable OpenGL main_window size={}x{} dpi={}",
                    dimensions.pixel_width, dimensions.pixel_height, dimensions.dpi
                ));
                Some(window.enable_opengl().await?)
            }
        };

        {
            let mut myself = tw.borrow_mut();
            myself.config_subscription.replace(config_subscription);
            if config.use_resize_increments {
                window.set_resize_increments(
                    ResizeIncrementCalculator {
                        x: myself.render_metrics.cell_size.width as u16,
                        y: myself.render_metrics.cell_size.height as u16,
                        padding_left: padding_left,
                        padding_top: padding_top,
                        padding_right: padding_right,
                        padding_bottom: padding_bottom,
                        border: border,
                        tab_bar_height: tab_bar_height,
                    }
                    .into(),
                );
            }

            if let Some(gl) = gl {
                myself.gl.replace(Rc::clone(&gl));
                myself.created(RenderContext::Glium(Rc::clone(&gl)))?;
            }
            if let Some(webgpu) = webgpu {
                myself.webgpu.replace(Rc::clone(&webgpu));
                // Seed the swapchain latency from the current focus: a
                // window spawned in the background starts at the reduced
                // latency and only pays for the third drawable once it is
                // actually focused.
                webgpu.set_desired_frame_latency(if myself.focused.is_some() { 2 } else { 1 });
                myself.created(RenderContext::WebGpu(Rc::clone(&webgpu)))?;
            }
            myself.apply_native_terminal_settings();
            myself.apply_workspace_thread_font_scales();
            myself.load_os_parameters();
            if restore_saved_thread {
                if let Some(thread_id) =
                    crate::workspace_threads::thread_to_restore_for_space(&myself.active_space_id)
                {
                    myself.activate_workspace_thread_for_new_window(
                        thread_id,
                        &window,
                        mux_window_id,
                    );
                } else {
                    myself.sync_current_workspace_thread();
                }
            } else {
                myself.sync_current_workspace_thread();
            }
            myself.maybe_show_onboarding();
            // The mux window can predate this native window, so no
            // TabAddedToWindow notification is guaranteed after the GUI has
            // subscribed. Converge the already-active tab before the first
            // visible frame; an unknown remote owner enters the opaque
            // takeover epoch here rather than exposing its saved/TUI grid.
            if !myself.content_view_foreground() {
                myself.resize_mux_tabs_to_current_terminal_size();
            }
            window.show();
            // Maximizing after the window is shown, rather than asking for a
            // maximized window up front: `show` is an ordinary "restore and
            // show" on Windows and would undo an earlier maximize. Doing it
            // this way round also leaves the rect we opened at as the one
            // Windows puts the window back at, so unmaximizing lands on the
            // size the window had before it was maximized.
            if restored_frame.map_or(false, |restored| restored.maximized) {
                window.maximize();
            }
            myself.subscribe_to_pane_updates();
            myself.emit_window_event("window-config-reloaded", None);
            myself.emit_status_event();
        }

        crate::update::start_update_checker();
        // Register with the window's *current* mux id, not the one we were
        // constructed with: restoring a saved thread above may have adopted
        // this window onto a different (already-live) mux window via
        // switch_to_mux_window. Recording the stale construction id would point
        // known_windows at a mux window the restore just orphaned (and possibly
        // killed), and the next reconcile would then close this brand-new window.
        let (adopted_mux_window_id, recovery_slot) = {
            let term_window = tw.borrow();
            (
                term_window.mux_window_id,
                term_window.frontend_recovery_slot(),
            )
        };
        front_end().record_known_window(window.clone(), adopted_mux_window_id, recovery_slot);

        // A Project directory refused before this window existed has been
        // waiting for somewhere to be shown; this is the first moment there is
        // one.
        window.notify(TermWindowNotif::Apply(Box::new(|tw| {
            tw.show_pending_project_root_problem()
        })));

        Ok(())
    }

    fn dispatch_window_event(
        &mut self,
        event: WindowEvent,
        window: &Window,
    ) -> anyhow::Result<bool> {
        log::debug!("{event:?}");
        match event {
            WindowEvent::Destroyed => {
                crate::main_window_placement::window_closed(self.mux_window_id);
                self.flush_right_sidebar_note_blocking();
                self.clear_gui_recovery_intent();
                // Ensure that we cancel any overlays we had running, so
                // that the mux can empty out, otherwise the mux keeps
                // the TermWindow alive via the frontend even though
                // the window is gone and we'll linger forever.
                // <https://github.com/wezterm/wezterm/issues/3522>
                crate::workspace_threads::release_window_space(self.space_owner_id);
                self.clear_all_overlays();
                Ok(false)
            }
            WindowEvent::CloseRequested => {
                // Get the placement on disk before anything that might quit
                // the process gets going. The claim is kept: the close may
                // still be called off by a confirmation prompt.
                crate::main_window_placement::close_requested(self.mux_window_id);
                self.close_requested(window);
                Ok(true)
            }
            WindowEvent::WindowFrameChanged { frame, maximized } => {
                // While a restored frame is still being asserted the window is
                // passing through geometry nobody chose; recording it would
                // save the startup wobble rather than where the window lives.
                if !self.settle_restored_frame(frame, maximized, window) {
                    crate::main_window_placement::record(self.mux_window_id, frame, maximized);
                }
                Ok(true)
            }
            WindowEvent::AppearanceChanged(appearance) => {
                log::debug!("Appearance is now {:?}", appearance);
                // This is a bit fugly; we get per-window notifications
                // for appearance changes which successfully updates the
                // per-window config, but we need to explicitly tell the
                // global config to reload, otherwise things that acces
                // the config via config::configuration() will see the
                // prior version of the config.
                // What's fugly about this is that we'll reload the
                // global config here once per window, which could
                // be nasty for folks with a lot of windows.
                // <https://github.com/wezterm/wezterm/issues/2295>
                config::reload();
                // The scheme the interface defaults to has a side (see
                // `native_settings::effective_color_scheme`), and the side just
                // moved. Under System theme with nothing picked, a window
                // opened in light appearance has the light scheme sitting in
                // its `config_overrides`; a reload alone keeps it there, so the
                // desktop going dark would leave a dark interface in front of a
                // light scheme's black-on-white text.
                //
                // Safe to recompute rather than remember: the only writer of
                // `color_scheme` in the overrides is this pairing and the
                // explicit picks it defers to, and `effective_color_scheme`
                // returns the pick unchanged.
                let reloaded = self.apply_color_scheme_override(
                    crate::native_settings::effective_color_scheme(
                        &crate::native_settings::load(),
                        &self.config,
                    ),
                );
                if !reloaded {
                    self.config_was_reloaded();
                }
                Ok(true)
            }
            WindowEvent::PerformKeyAssignment(action) => {
                // A native context menu delivers its selection this way;
                // whichever way the menu ended, it is over now.
                self.native_context_menu_open = false;
                if let Some(pane) = self.get_active_pane_or_overlay() {
                    self.perform_key_assignment(&pane, &action)?;
                    window.invalidate();
                }
                Ok(true)
            }
            WindowEvent::PerformContextMenuAction(action_id) => {
                self.native_context_menu_open = false;
                self.perform_context_menu_application_action(action_id);
                window.invalidate();
                Ok(true)
            }
            WindowEvent::ContextMenuDismissed => {
                self.native_context_menu_open = false;
                self.context_menu_was_dismissed();
                window.invalidate();
                Ok(true)
            }
            WindowEvent::FocusChanged(focused) => {
                self.focus_changed(focused, window);
                Ok(true)
            }
            WindowEvent::OcclusionChanged(visible) => {
                self.occlusion_changed(visible, window);
                Ok(true)
            }
            WindowEvent::MouseEvent(event) => {
                self.mouse_event_impl(event, window);
                Ok(true)
            }
            WindowEvent::ToggleWorkspaceSidebar => {
                self.toggle_workspace_sidebar();
                let dimensions = self.dimensions;
                self.apply_dimensions(&dimensions, None, window);
                window.invalidate();
                Ok(true)
            }
            WindowEvent::WorkspaceSidebarButtonHover(hovering) => {
                self.titlebar_sidebar_button_hovered = hovering;
                // Step the hover machine exactly like a mouse event would;
                // the frame it asks for registers any dwell wakeup.
                let input = self.workspace_sidebar_hover_input();
                if self
                    .workspace_sidebar_hover
                    .step(input, std::time::Instant::now())
                    != crate::termwindow::sidebar_hover::HoverFrame::None
                {
                    window.invalidate();
                }
                Ok(true)
            }
            WindowEvent::MouseLeave => {
                self.mouse_leave_impl(window);
                Ok(true)
            }
            WindowEvent::Resized {
                dimensions,
                window_state,
                live_resizing,
            } => {
                // Switching Spaces can synchronously trigger a layout/size
                // reconciliation. Preserve the already-committed sidebar
                // transition through that internal resize; otherwise the
                // source and target pages snap before the first animation
                // frame. Interactive/uncommitted gestures are still cancelled
                // when the user actually resizes the window.
                if !self.workspace_sidebar_swipe.is_committing_or_committed() {
                    self.workspace_sidebar_swipe.cancel_immediately();
                    self.clear_workspace_space_swipe_frame_transition();
                }
                // Land a travelling hover reveal rather than deleting it: a
                // settled overlay survives a resize cleanly, a mid-flight one
                // would jump.
                self.workspace_sidebar_hover.settle_immediately();
                self.resize(dimensions, window_state, window, live_resizing);
                Ok(true)
            }
            WindowEvent::SetInnerSizeCompleted => {
                self.resizes_pending -= 1;
                if self.is_repaint_pending {
                    self.is_repaint_pending = false;
                    if self.webgpu.is_some() {
                        self.do_paint_webgpu()?;
                    } else {
                        self.do_paint(window);
                    }
                }
                self.apply_pending_scale_changes();
                Ok(true)
            }
            WindowEvent::AdviseModifiersLedStatus(modifiers, leds) => {
                self.current_modifier_and_leds = (modifiers, leds);
                self.update_title();
                window.invalidate();
                Ok(true)
            }
            WindowEvent::RawKeyEvent(event) => {
                self.raw_key_event_impl(event, window);
                Ok(true)
            }
            WindowEvent::KeyEvent(event) => {
                self.key_event_impl(event, window);
                Ok(true)
            }
            WindowEvent::AdviseDeadKeyStatus(status) => {
                if self.config.debug_key_events {
                    log::info!("DeadKeyStatus now: {:?}", status);
                } else {
                    log::trace!("DeadKeyStatus now: {:?}", status);
                }
                self.dead_key_status = status;
                self.update_title();
                // Ensure that we repaint so that any composing
                // text is updated
                window.invalidate();
                Ok(true)
            }
            WindowEvent::NativeTextInputReplace {
                token,
                revision,
                source_range,
                text,
            } => {
                if self.right_sidebar_note.view.focused
                    && self.right_sidebar_note.native_text_input_token == token
                {
                    if let Some(session) = self.right_sidebar_note.session.clone() {
                        if session.lock().replace_range_at_revision(
                            &mut self.right_sidebar_note.view,
                            revision,
                            source_range,
                            &text,
                        ) {
                            self.note_did_edit();
                        }
                    }
                }
                window.invalidate();
                Ok(true)
            }
            WindowEvent::NeedRepaint => {
                if self.resizes_pending > 0 {
                    self.is_repaint_pending = true;
                    Ok(true)
                } else if self.webgpu.is_some() {
                    self.do_paint_webgpu()
                } else {
                    Ok(self.do_paint(window))
                }
            }
            WindowEvent::Notification(item) => {
                if let Ok(notif) = item.downcast::<TermWindowNotif>() {
                    self.dispatch_notif(*notif, window)
                        .context("dispatch_notif")?;
                }
                Ok(true)
            }
            WindowEvent::DroppedString(text) => {
                let pane = match self.get_active_pane_or_overlay() {
                    Some(pane) => pane,
                    None => return Ok(true),
                };
                pane.send_paste(text.as_str())?;
                Ok(true)
            }
            WindowEvent::DroppedUrl(urls) => {
                let pane = match self.get_active_pane_or_overlay() {
                    Some(pane) => pane,
                    None => return Ok(true),
                };
                let urls = urls
                    .iter()
                    .map(|url| self.config.quote_dropped_files.escape(&url.to_string()))
                    .collect::<Vec<_>>()
                    .join(" ")
                    + " ";
                pane.send_paste(urls.as_str())?;
                Ok(true)
            }
            WindowEvent::DroppedFile { paths, coords } => {
                if self.right_sidebar_mode == RightSidebarMode::Tasks
                    && self.right_sidebar_note.view.focused
                {
                    let Some(document) = self.right_sidebar_note.document.clone() else {
                        log::warn!(
                            "cannot import a Note attachment before a Vault document is open"
                        );
                        return Ok(true);
                    };
                    let paths = paths.clone();
                    let notify_window = window.clone();
                    promise::spawn::spawn(async move {
                        let markdown = promise::spawn::spawn_into_new_thread(move || {
                            let mut inserted = Vec::new();
                            for path in paths {
                                match crate::markdown_editor::import_attachment(&document, &path) {
                                    Ok(relative) => {
                                        let alt = path
                                            .file_stem()
                                            .and_then(|name| name.to_str())
                                            .unwrap_or("image")
                                            .replace('\\', "\\\\")
                                            .replace('[', "\\[")
                                            .replace(']', "\\]")
                                            .replace(['\r', '\n'], " ");
                                        inserted.push(format!("![{alt}]({relative})"));
                                    }
                                    Err(err) => {
                                        log::warn!(
                                            "unable to import Note attachment {}: {err:#}",
                                            path.display()
                                        );
                                    }
                                }
                            }
                            anyhow::Ok(inserted.join("\n"))
                        })
                        .await;
                        if let Ok(markdown) = markdown {
                            if markdown.is_empty() {
                                return;
                            }
                            notify_window.notify(TermWindowNotif::Apply(Box::new(
                                move |term_window| {
                                    if term_window.push_right_sidebar_text(&markdown) {
                                        term_window.invalidate_window();
                                    }
                                },
                            )));
                        }
                    })
                    .detach();
                    return Ok(true);
                }
                // Aimed at the Files tree: transfer rather than paste the
                // paths into a shell. Remote uploads over SFTP, local copies
                // straight into the hovered directory.
                if self.upload_dropped_files_to_remote(&paths, coords) {
                    return Ok(true);
                }
                // The walk runs on a worker, so any conflict prompt is raised
                // when it comes back rather than here.
                if self.copy_dropped_files_into_local_panel(&paths, coords) {
                    return Ok(true);
                }
                // A drop on the terminal of a REMOTE session: pasting a local
                // path there would name a file the server does not have, so
                // upload first and paste the remote path instead.
                if self.upload_dropped_files_to_remote_terminal(&paths, coords) {
                    return Ok(true);
                }
                let pane = match self.get_active_pane_or_overlay() {
                    Some(pane) => pane,
                    None => return Ok(true),
                };
                let paths = paths
                    .iter()
                    .map(|path| {
                        self.config
                            .quote_dropped_files
                            .escape(&path.to_string_lossy())
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
                    + " ";
                pane.send_paste(&paths)?;
                Ok(true)
            }
            WindowEvent::DraggedFile { coords, .. } => {
                self.update_right_sidebar_remote_drop_target(coords);
                self.update_right_sidebar_local_drop_target(coords);
                Ok(true)
            }
            WindowEvent::DragLeave => {
                self.clear_right_sidebar_remote_drop_target();
                self.clear_right_sidebar_local_drop_target();
                Ok(true)
            }
        }
    }

    /// Put a restored window back on the frame it reopened at, if startup has
    /// moved it off. Returns true while that is still in progress.
    ///
    /// Opening at the remembered rect is not enough on its own: the first real
    /// resize event makes the GUI recompute its pixel size from the font
    /// metrics and the configured rows and columns and ask the window for
    /// *that*, which throws away the size we just reopened at. Suppressing
    /// that recalculation is the wrong fix -- it is what keeps the terminal's
    /// shape across a dpi change -- so the remembered frame is simply asserted
    /// again afterwards.
    ///
    /// Each correction produces another frame-changed event, so this settles
    /// as soon as the frame matches; the attempt budget keeps it from becoming
    /// a tug of war with a backend that will not take the rect. A window that
    /// comes back maximized is left alone: its frame is the screen's, and the
    /// rect we opened at is already its restore rect.
    fn settle_restored_frame(
        &mut self,
        frame: ScreenRect,
        maximized: bool,
        window: &Window,
    ) -> bool {
        let Some(target) = self.restored_frame_target else {
            return false;
        };

        let expired = self
            .restored_frame_deadline
            .map_or(true, |deadline| Instant::now() > deadline);

        if maximized || frame == target || self.restored_frame_attempts == 0 || expired {
            if frame != target && !maximized {
                log::warn!(
                    "gave up restoring the main window frame to {target:?}; it settled at {frame:?}"
                );
            }
            self.restored_frame_target = None;
            self.restored_frame_deadline = None;
            return false;
        }

        self.restored_frame_attempts -= 1;
        log::trace!("re-asserting restored main window frame {target:?}, saw {frame:?}");
        window.set_frame_rect(target);
        true
    }

    fn do_paint(&mut self, window: &Window) -> bool {
        // A repaint of a hidden window (a resize completing, a config
        // reload) regrows what the occlusion release dropped; mark the
        // episode dirty so the heartbeat releases again.
        if self.occluded.is_some() {
            self.occlusion_released = false;
        }
        let gl = match self.gl.as_ref() {
            Some(gl) => gl,
            None => return false,
        };

        if gl.is_context_lost() {
            log::error!("opengl context was lost; should reinit");
            window.close();
            front_end().forget_known_window(window);
            return false;
        }

        let mut frame = glium::Frame::new(
            Rc::clone(&gl),
            (
                self.dimensions.pixel_width as u32,
                self.dimensions.pixel_height as u32,
            ),
        );
        let outcome = match self.paint_impl(&mut RenderFrame::Glium(&mut frame)) {
            Ok(outcome) => outcome,
            Err(err) => {
                log::error!("failed to draw OpenGL frame: {err:#}");
                self.discard_unpresented_pane_output();
                return false;
            }
        };
        if !outcome.draw_submitted {
            self.discard_unpresented_pane_output();
            return false;
        }
        let presented = window.finish_frame(frame).is_ok();
        if frame_can_acknowledge_output(outcome, presented) {
            self.acknowledge_presented_pane_output();
        } else {
            self.discard_unpresented_pane_output();
        }
        presented
    }

    fn do_paint_webgpu(&mut self) -> anyhow::Result<bool> {
        // See do_paint: a hidden repaint regrows released caches.
        if self.occluded.is_some() {
            self.occlusion_released = false;
        }
        self.webgpu.as_mut().unwrap().resize(self.dimensions);
        match self.do_paint_webgpu_impl() {
            Ok(ok) => Ok(ok),
            Err(err) => {
                self.discard_unpresented_pane_output();
                match err.downcast_ref::<wgpu::SurfaceError>() {
                    Some(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                        self.webgpu.as_mut().unwrap().resize(self.dimensions);
                        return self.do_paint_webgpu_impl();
                    }
                    _ => {}
                }
                Err(err)
            }
        }
    }

    fn do_paint_webgpu_impl(&mut self) -> anyhow::Result<bool> {
        let outcome = self.paint_impl(&mut RenderFrame::WebGpu)?;
        if frame_can_acknowledge_output(outcome, outcome.draw_submitted) {
            self.acknowledge_presented_pane_output();
        } else {
            self.discard_unpresented_pane_output();
        }
        Ok(outcome.draw_submitted)
    }

    fn dispatch_notif(&mut self, notif: TermWindowNotif, window: &Window) -> anyhow::Result<()> {
        fn chan_err<T>(e: smol::channel::TrySendError<T>) -> anyhow::Error {
            anyhow::anyhow!("{}", e)
        }

        match notif {
            TermWindowNotif::InvalidateShapeCacheForChars(chars) => {
                // The generation bump retires the terminal line caches
                // (their rendered runs may hold placeholder glyphs for these
                // codepoints); the shape caches evict selectively so lines
                // without the new codepoints stay warm.
                self.shape_generation += 1;
                self.shape_cache
                    .borrow_mut()
                    .retain(|key, _| !key.text.chars().any(|c| chars.contains(&c)));
                self.ui_shape_caches.borrow_mut().evict_containing(&chars);
                self.publish_ui_shape_cache_diagnostics();
                self.invalidate_modal();
                window.invalidate();
            }
            TermWindowNotif::InvalidateTerminalShapeCache => {
                // Bumping the generation retires every color-bearing terminal
                // cache entry (LineToEleShapeCacheKey, PreviewQuadKey carry
                // it). The UI text caches stay: their keys have no colors.
                self.shape_generation += 1;
                self.shape_cache.borrow_mut().clear();
                self.invalidate_modal();
                window.invalidate();
            }
            TermWindowNotif::PerformAssignment {
                pane_id,
                assignment,
                tx,
            } => {
                let mux = Mux::get();
                let result = || -> anyhow::Result<()> {
                    // The CopyMode overlay doesn't exist in the mux, but aliases
                    // itself with the overlaid pane's pane_id.
                    // So we do a bit of fancy footwork here to resolve the overlay
                    // and use that if it has the same pane_id, but otherwise fall
                    // back to what we get from the mux.
                    // <https://github.com/wezterm/wezterm/issues/3209>
                    let active_pane = self
                        .get_active_pane_or_overlay()
                        .ok_or_else(|| anyhow!("there is no active pane!?"))?;
                    let pane = if active_pane.pane_id() == pane_id {
                        active_pane
                    } else {
                        mux.get_pane(pane_id)
                            .ok_or_else(|| anyhow!("pane id {} is not valid", pane_id))?
                    };
                    self.perform_key_assignment(&pane, &assignment)
                        .context("perform_key_assignment")?;
                    Ok(())
                }();
                window.invalidate();
                if let Some(tx) = tx {
                    tx.try_send(result).ok();
                }
            }
            TermWindowNotif::SetRightStatus(status) => {
                if status != self.right_status {
                    self.right_status = status;
                    self.update_title_post_status();
                } else {
                    self.schedule_next_status_update();
                }
            }
            TermWindowNotif::SetLeftStatus(status) => {
                if status != self.left_status {
                    self.left_status = status;
                    self.update_title_post_status();
                } else {
                    self.schedule_next_status_update();
                }
            }
            TermWindowNotif::GetDimensions(tx) => {
                tx.try_send((self.dimensions, self.window_state))
                    .map_err(chan_err)
                    .context("send GetDimensions response")?;
            }
            TermWindowNotif::GetTerminalSize(tx) => {
                tx.try_send(self.terminal_size)
                    .map_err(chan_err)
                    .context("send GetTerminalSize response")?;
            }
            TermWindowNotif::GetEffectiveConfig(tx) => {
                tx.try_send(self.config.clone())
                    .map_err(chan_err)
                    .context("send GetEffectiveConfig response")?;
            }
            TermWindowNotif::FinishWindowEvent { name, again } => {
                self.finish_window_event(&name, again);
            }
            TermWindowNotif::GetConfigOverrides(tx) => {
                tx.try_send(self.config_overrides.clone())
                    .map_err(chan_err)
                    .context("send GetConfigOverrides response")?;
            }
            TermWindowNotif::SetConfigOverrides(value) => {
                if value != self.config_overrides {
                    self.config_overrides = value;
                    self.config_was_reloaded();
                }
            }
            TermWindowNotif::CancelOverlayForPane(pane_id) => {
                self.cancel_overlay_for_pane(pane_id);
            }
            TermWindowNotif::CancelOverlayForTab { tab_id, pane_id } => {
                self.cancel_overlay_for_tab(tab_id, pane_id);
            }
            TermWindowNotif::MuxNotification(n) => match n {
                MuxNotification::Alert {
                    alert: Alert::SetUserVar { name, value },
                    pane_id,
                } => {
                    self.refresh_thread_work_for_pane(pane_id);
                    self.emit_user_var_event(pane_id, name, value);
                }
                MuxNotification::WindowTitleChanged { .. }
                | MuxNotification::Alert {
                    alert: Alert::OutputSinceFocusLost | Alert::CurrentWorkingDirectoryChanged,
                    ..
                } => {
                    self.update_title();
                }
                MuxNotification::Alert {
                    alert:
                        Alert::WindowTitleChanged(_)
                        | Alert::TabTitleChanged(_)
                        | Alert::IconTitleChanged(_)
                        | Alert::Progress(_),
                    pane_id,
                } => {
                    self.refresh_thread_work_for_pane(pane_id);
                    self.update_title();
                }
                MuxNotification::Alert {
                    alert: Alert::PaletteChanged,
                    pane_id: _,
                } => {
                    // Terminal-side shape caches include resolved palette
                    // colors, so invalidate them — but only them. The UI text
                    // caches are colorless and clearing them here made every
                    // OSC palette write re-shape the whole sidebar. The
                    // handler already ends in an unconditional
                    // window.invalidate(), so no separate repaint request is
                    // needed (and none of this may be throttled: colors
                    // changing must show even on an unfocused window).
                    self.dispatch_notif(TermWindowNotif::InvalidateTerminalShapeCache, window)?;
                }
                MuxNotification::Alert {
                    alert: Alert::Bell,
                    pane_id,
                } => {
                    if !self.window_contains_pane(pane_id) {
                        return Ok(());
                    }

                    match self.config.audible_bell {
                        AudibleBell::SystemBeep => {
                            Connection::get().expect("on main thread").beep();
                        }
                        AudibleBell::Disabled => {}
                    }

                    log::trace!("Ding! (this is the bell) in pane {}", pane_id);
                    self.emit_window_event("bell", Some(pane_id));

                    let mut per_pane = self.pane_state(pane_id);
                    per_pane.bell_start.replace(Instant::now());
                    window.invalidate();
                }
                MuxNotification::Alert {
                    alert: Alert::ToastNotification { .. },
                    ..
                } => {}
                MuxNotification::TabAddedToWindow {
                    window_id: _,
                    tab_id,
                } => {
                    // Before anything paints it: a tab attached rather than
                    // spawned carries the global configuration, not this
                    // window's colour scheme.
                    self.push_window_config_to_tab(tab_id);
                    let mux = Mux::get();
                    if let Some(tab) = mux.get_tab(tab_id) {
                        let is_remote_thinkterm_tab = tab.get_active_pane().is_some_and(|pane| {
                            pane.downcast_ref::<wezterm_client::pane::ClientPane>()
                                .is_some()
                        }) && mux
                            .get_window(self.mux_window_id)
                            .is_some_and(|mux_window| {
                                let workspace = mux_window.get_workspace();
                                crate::workspace_threads::is_thread_workspace_name(workspace)
                                    || crate::workspace_threads::workspace_has_thread_binding(
                                        workspace,
                                    )
                            });

                        if is_remote_thinkterm_tab {
                            // Bind this notification to its own tab. A
                            // background remote tab keeps the server's
                            // canonical geometry; only the tab currently being
                            // drawn is converged to this GUI.
                            if mux
                                .get_active_tab_for_window(self.mux_window_id)
                                .is_some_and(|active| active.tab_id() == tab_id)
                            {
                                self.stage_workspace_thread_font_scales();
                                self.sync_active_tab_geometry_now();
                            }
                        } else {
                            // Preserve upstream behavior for ordinary WezTerm
                            // domains. ThinkTerm thread tabs deliberately do
                            // not enlarge the OS window to a remote renderer's
                            // historical rows/cols.
                            let mut size = self.terminal_size;
                            let tab_size = tab.get_size();
                            size.rows = size.rows.max(tab_size.rows);
                            size.cols = size.cols.max(tab_size.cols);

                            if size.rows != self.terminal_size.rows
                                || size.cols != self.terminal_size.cols
                                || size.pixel_width != self.terminal_size.pixel_width
                                || size.pixel_height != self.terminal_size.pixel_height
                            {
                                self.set_window_size(size, window)?;
                            } else if tab_size != self.terminal_size {
                                tab.resize(self.terminal_size);
                            }
                            self.stage_workspace_thread_font_scales();
                            self.force_sync_active_mux_tab_pane_sizes();
                        }
                    }
                    self.persist_workspace_layout_after_mutation("tab added");
                }
                MuxNotification::PaneOutput(pane_id) => {
                    self.mux_pane_output_event(pane_id);
                }
                MuxNotification::AgentStatusChanged(pane_id) => {
                    // The panel snapshot is cached briefly; a real change
                    // must not wait out that TTL.
                    crate::agent_status::invalidate_agent_pane_cache();
                    self.refresh_thread_work_for_pane(pane_id);
                    if self.right_sidebar_mode == RightSidebarMode::Agents {
                        // The thread status may be unchanged while the
                        // per-pane chip flipped (e.g. Idle→Working inside an
                        // already-Running thread); repaint the open panel.
                        if let Some(win) = self.window.as_ref() {
                            win.invalidate();
                        }
                    }
                }
                MuxNotification::WindowInvalidated(_) => {
                    window.invalidate();
                    self.update_title_post_status();
                }
                MuxNotification::WindowRemoved(_window_id) => {
                    // Handled by frontend
                }
                MuxNotification::AssignClipboard { .. } => {
                    // Handled by frontend
                }
                MuxNotification::SaveToDownloads { .. } => {
                    // Handled by frontend
                }
                MuxNotification::DefaultPaletteChanged => {}
                MuxNotification::ThinkTermTreeChanged => {
                    // Server-side only; the remote tree reaches this process
                    // as a pushed ThinkTermTreeState PDU instead.
                }
                MuxNotification::ThinkTermSessionChanged => {
                    // Consumed by mux-server connections attached to this GUI.
                }
                MuxNotification::FrontendLeaseChanged(state) => {
                    let mux = Mux::get();
                    if mux.window_containing_tab(state.tab_id) == Some(self.mux_window_id) {
                        if self.owns_frontend_viewport() {
                            self.resize_mux_tabs_to_current_terminal_size();
                        }
                        window.invalidate();
                    }
                }
                MuxNotification::FrontendAccessChanged(_) => {
                    if self.owns_frontend_viewport() {
                        self.resize_mux_tabs_to_current_terminal_size();
                    }
                    self.update_title_post_status();
                    window.invalidate();
                }
                MuxNotification::PaneFocused(pane_id) => {
                    // Also handled by clientpane
                    self.refresh_thread_work_for_pane(pane_id);
                    self.update_title_post_status();
                }
                MuxNotification::TabResized(_) => {
                    // Also handled by wezterm-client
                    self.update_title_post_status();
                    // A split lands here: PaneAdded goes out before the new
                    // pane is in the tab tree, and a mirror of a server-side
                    // split never sends it at all, so the snapshot "pane added"
                    // takes sees the old layout. Sizes are not part of the
                    // fingerprint, so live resizes cost a tree walk and no
                    // write.
                    self.persist_workspace_layout_if_structure_changed();
                }
                MuxNotification::TabTitleChanged { .. } => {
                    self.update_title_post_status();
                }
                MuxNotification::PaneAdded(pane_id) => {
                    self.push_window_config_to_pane(pane_id);
                    self.refresh_thread_work_for_pane(pane_id);
                    self.persist_workspace_layout_after_mutation("pane added");
                }
                MuxNotification::PaneRemoved(_) => {
                    self.refresh_all_thread_work();
                    self.persist_workspace_layout_after_mutation("pane removed");
                }
                MuxNotification::WorkspaceRenamed { .. }
                | MuxNotification::WindowWorkspaceChanged(_)
                | MuxNotification::ActiveWorkspaceChanged(_) => {
                    self.sync_current_workspace_thread();
                }
                MuxNotification::WindowCreated(_) => {
                    // Remote windows folded in by an attach/resync may
                    // reference sidebar records this client lost; rebuild
                    // them so running remote terminals stay reachable.
                    if crate::workspace_threads::adopt_orphan_remote_thread_windows(
                        &self.active_space_id,
                    ) {
                        window.invalidate();
                    }
                    self.sync_current_workspace_thread();
                }
                MuxNotification::Empty => {}
            },
            TermWindowNotif::EmitStatusUpdate => {
                // Re-arm before doing any work: this tick drives the render
                // watchdog, and a panic below (swallowed by the spawn queue)
                // must not end the heartbeat for the window's lifetime. The
                // later re-arm from the title path dedupes via
                // last_status_call.
                self.schedule_next_status_update();
                self.emit_status_event();
                self.refresh_all_thread_work();
                self.terminal_render_watchdog();
                self.maybe_release_occluded_memory();
            }
            TermWindowNotif::OpenProjectPath(path) => {
                let path = path.to_string_lossy();
                // In a mux-domain Space the path names a directory on the
                // remote server and must not be resolved locally.
                let result =
                    if crate::workspace_threads::client_domain_for_space(&self.active_space_id)
                        .is_some()
                    {
                        crate::workspace_threads::create_remote_project_from_path(
                            &self.active_space_id,
                            path.as_ref(),
                        )
                    } else {
                        crate::workspace_threads::create_project_from_path(
                            &self.active_space_id,
                            path.as_ref(),
                        )
                    };
                match result {
                    Ok(thread_id) => self.activate_workspace_thread(thread_id, window),
                    Err(err) => log::error!("failed to create ThinkTerm project: {err:#}"),
                }
            }
            TermWindowNotif::GetSelectionForPane { pane_id, tx } => {
                let mux = Mux::get();
                let pane = mux
                    .get_pane(pane_id)
                    .ok_or_else(|| anyhow!("pane id {} is not valid", pane_id))?;

                tx.try_send(self.selection_text(&pane))
                    .map_err(chan_err)
                    .context("send GetSelectionForPane response")?;
            }
            TermWindowNotif::Apply(func) => {
                func(self);
            }
            TermWindowNotif::SwitchToMuxWindow(mux_window_id) => {
                self.switch_to_mux_window(mux_window_id);
            }
            TermWindowNotif::SetInnerSize { width, height } => {
                self.set_inner_size(window, width, height);
            }
        }

        Ok(())
    }

    fn set_inner_size(&mut self, window: &Window, width: usize, height: usize) {
        self.resizes_pending += 1;
        window.set_inner_size(width, height);
    }

    /// Re-point THIS GUI window at a different mux window (the mux window of
    /// the Space/workspace we want to display) without disturbing any other
    /// window. This is the per-window primitive used both by the
    /// `SwitchToMuxWindow` notification and by in-place Space/thread switches.
    pub(crate) fn switch_to_mux_window(&mut self, mux_window_id: MuxWindowId) {
        if self.mux_window_id == mux_window_id
            && front_end()
                .gui_window_for_mux_window(mux_window_id)
                .is_some()
        {
            // Already showing it and the mapping is current; a deferred
            // Space-switch reflow can settle now since there is no other
            // window to protect from the resize.
            self.consume_pending_sidebar_reflow();
            self.sync_active_tab_geometry_now();
            return;
        }

        self.cancel_remote_divider_resizes_except(None);
        self.mux_window_id = mux_window_id;
        *self.mux_window_id_for_subscriptions.lock().unwrap() = mux_window_id;

        // Re-subscribe only now that the shared id names the adopted mux
        // window. Subscribing any earlier (recover_from_dead_mux_window
        // used to) loses a race with the queued WindowRemoved for the mux
        // window being left: the notification compares against the shared
        // id, still finds a match, and kills the brand-new subscription —
        // leaving a live window that never hears PaneOutput again, painting
        // only on input events and the 1s render watchdog.
        self.subscribe_to_pane_updates();

        // Keep the frontend's window<->mux mapping accurate so that the
        // additive reconcile does not try to spawn a duplicate window for the
        // mux window we just adopted.
        if let Some(window) = self.window.as_ref() {
            front_end().rebind_known_window(window, mux_window_id);
        }

        self.sync_content_view_surfaces_with_mux();
        self.clear_all_overlays();
        self.current_highlight.take();
        self.invalidate_fancy_tab_bar();
        self.invalidate_modal();

        // The adopted window's panes were built under the mux window this
        // GUI window is leaving -- or, for the local session server, under
        // no GUI window at all -- and carry the global configuration; this
        // window's colour scheme has to reach them the same way it reaches a
        // tab that is added to it.
        self.push_window_config_to_all_tabs();

        // Restore every destination-side geometry input before sizing panes.
        // consume_pending_sidebar_reflow can synchronously apply dimensions,
        // so persisted font scales must already be present before it runs.
        // Otherwise a remote TUI can receive an intermediate SIGWINCH followed
        // by the final one and corrupt its redraw.
        self.stage_workspace_thread_font_scales();

        // The destination window is adopted; a Space switch that changed the
        // sidebar width can resize the terminal now without touching the
        // window being left. ClientPane and Tab both no-op the explicit final
        // sync below if this already imposed exactly the same geometry.
        self.consume_pending_sidebar_reflow();

        // Tab::resize updates the local split tree but deliberately skips
        // remote mirror panes. Force the final per-pane GUI-sized resize now
        // rather than waiting for a later paint (a connection view may still
        // be up).
        self.resize_mux_tabs_to_current_terminal_size();
        self.sync_current_workspace_thread();

        // A Space/thread switch can replace the active pane without changing
        // the native window's focus state, so no FocusChanged event follows
        // to focus the newly adopted pane. Do that handoff immediately rather
        // than requiring the user to click the terminal first.
        if self.focused.is_some() {
            if let Some(pane) = self.get_active_pane_or_overlay() {
                pane.advise_focus();
                Mux::get().record_focus_for_current_identity(pane.pane_id());
            }
        }

        self.update_title();
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub(crate) fn active_space_id(&self) -> &str {
        &self.active_space_id
    }


    /// The mux window this GUI window displayed is gone (its last pane
    /// exited, e.g. `exit` in a thread's only shell). Fall back to another
    /// thread of the Space — or a fresh one — instead of dying with it,
    /// which used to take the whole app down when this was the last window.
    /// Incidental windows (reconnect/auth prompts) and remote mux Spaces
    /// keep the close-with-the-mux-window behavior.
    pub(crate) fn recover_from_dead_mux_window(&mut self) {
        let mux = Mux::get();
        if mux.get_window(self.mux_window_id).is_some() {
            // Already re-pointed at a live mux window (a thread switch or
            // Space delete raced the reconcile); nothing to recover.
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };
        let recovery_thread = if self.dies_with_mux_window
            || crate::workspace_threads::client_domain_for_space(&self.active_space_id).is_some()
        {
            None
        } else {
            crate::workspace_threads::thread_to_recover_after_window_death(&self.active_space_id)
        };
        let Some(thread_id) = recovery_thread else {
            window.close();
            front_end().forget_known_window(&window);
            return;
        };
        // The mux subscription cancelled itself when our mux window was
        // removed. Deliberately NOT re-established here: the shared
        // subscription id still names the dead mux window, so a queued
        // WindowRemoved would match and kill a subscription made now.
        // switch_to_mux_window re-subscribes after updating the id.
        self.activate_workspace_thread(thread_id, &window);
    }

    /// The sidebar Reconnect button: bring the active Space's mux domain
    /// back. A parked retry loop (gave up after continuous failure) is
    /// resumed in place; a detached or never-connected domain is attached
    /// fresh, registering it from the SSH host store when needed.
    pub(crate) fn reconnect_space_domain(&mut self) {
        let Some(domain_name) =
            crate::workspace_threads::client_domain_for_space(&self.active_space_id)
        else {
            return;
        };
        if !self.space_reconnects_in_flight.insert(domain_name.clone()) {
            return;
        }
        let Some(window) = self.window.clone() else {
            self.space_reconnects_in_flight.remove(&domain_name);
            return;
        };
        let mux_window_id = self.mux_window_id;
        promise::spawn::spawn(async move {
            let result = async {
                let mux = Mux::get();
                let domain = match mux.get_domain_by_name(&domain_name) {
                    Some(domain) => domain,
                    None => crate::connect_domain_from_ssh_host(&domain_name)?,
                };
                if let Some(client) = domain.downcast_ref::<ClientDomain>() {
                    if client.is_reconnect_suspended() {
                        client.resume_reconnect();
                        return anyhow::Ok(());
                    }
                }
                if domain.state() != mux::domain::DomainState::Attached {
                    // The spinner row (space_reconnects_in_flight) stays lit
                    // for the whole retry sequence; Cancelled just falls
                    // through to the cleanup notify below.
                    let ui =
                        mux::connui::ConnectionUI::with_params(mux::connui::ConnectionUIParams {
                            window_id: Some(mux_window_id),
                            ..Default::default()
                        });
                    crate::attach_domain_with_retry(
                        domain,
                        Some(mux_window_id),
                        ui,
                        move || Mux::get().get_window(mux_window_id).is_some(),
                        Some(std::time::Duration::from_secs(60)),
                    )
                    .await?;
                }
                anyhow::Ok(())
            }
            .await;
            let domain_name_for_cleanup = domain_name.clone();
            if let Err(err) = result {
                log::error!("reconnect {domain_name}: {err:#}");
            }
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window
                    .space_reconnects_in_flight
                    .remove(&domain_name_for_cleanup);
                if let Some(window) = term_window.window.as_ref() {
                    window.invalidate();
                }
            })));
        })
        .detach();
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// Switch THIS window to display the (already-live) workspace `workspace`,
    /// adopting its mux window in place, and make it the active workspace so
    /// that spawn-new-window defaults stay correct. Returns false when the
    /// workspace has no mux window to adopt.
    pub(crate) fn adopt_workspace_in_this_window(&mut self, workspace: &str) -> bool {
        let mux = Mux::get();
        let Some(target) = crate::workspace_threads::window_to_show_in_workspace(workspace)
        else {
            return false;
        };
        mux.set_active_workspace(workspace);
        self.switch_to_mux_window(target);
        true
    }

    /// Take care to remove our panes from the mux, otherwise
    /// we can leave the mux with no windows but some panes
    /// and it won't believe that we are empty.
    fn clear_all_overlays(&mut self) {
        let overlay_panes_to_cancel = self
            .pane_state
            .borrow()
            .iter()
            .filter_map(|(_, state)| state.overlay.as_ref().map(|overlay| overlay.pane.pane_id()))
            .collect::<Vec<_>>();

        for pane_id in overlay_panes_to_cancel {
            self.cancel_overlay_for_pane(pane_id);
        }

        let tab_overlays_to_cancel = self
            .tab_state
            .borrow()
            .iter()
            .filter_map(|(tab_id, state)| state.overlay.as_ref().map(|_| *tab_id))
            .collect::<Vec<_>>();

        for tab_id in tab_overlays_to_cancel {
            self.cancel_overlay_for_tab(tab_id, None);
        }

        self.pane_state.borrow_mut().clear();
        self.tab_state.borrow_mut().clear();
    }

    fn apply_icon(window: &Window) -> anyhow::Result<()> {
        #[cfg(target_os = "macos")]
        if let Some(path) = crate::native_settings::app_icon_path(
            crate::native_settings::load().appearance.app_icon,
        ) {
            match ::window::set_application_icon_from_file(&path) {
                Ok(()) => return Ok(()),
                Err(err) => log::warn!(
                    "failed to load macOS application icon from {}: {err:#}",
                    path.display()
                ),
            }
        }

        let image = image::load_from_memory(ICON_DATA)?.into_rgba8();
        let (width, height) = image.dimensions();
        window.set_icon(Image::with_rgba32(
            width as usize,
            height as usize,
            width as usize * 4,
            image.as_raw(),
        ));
        Ok(())
    }

    fn schedule_status_update(&self) {
        if let Some(window) = self.window.as_ref() {
            window.notify(TermWindowNotif::EmitStatusUpdate);
        }
    }

    fn is_pane_visible_in_tab(&mut self, tab: &Arc<Tab>, pane_id: PaneId) -> bool {
        let tab_id = tab.tab_id();
        if let Some(tab_overlay) = self
            .tab_state(tab_id)
            .overlay
            .as_ref()
            .map(|overlay| overlay.pane.clone())
        {
            return tab_overlay.pane_id() == pane_id;
        }

        tab.contains_pane(pane_id)
    }

    fn mux_pane_output_event(&mut self, pane_id: PaneId) {
        metrics::histogram!("mux.pane_output_event.rate").record(1.);

        // An armed trailing-edge timer means this window already owes
        // itself a repaint that will cover this output; nothing new to
        // decide. Checked before anything else because a flooding pane
        // lands here for every chunk of output. Only the cheap
        // full-rate exemptions are probed here — the ones that are
        // field reads. Focus clears the latch in focus_changed, and
        // entering an overlay clears it in assign_overlay*, so a
        // trusted latch genuinely implies the throttle still applies.
        //
        // A stale or newly-exempt latch is deliberately NOT cleared
        // here: clearing is only safe where a repaint decision is
        // reached below, or an invisible pane's event would cancel the
        // promised trailing repaint and nothing would honor it.
        if let Some(due) = self.unfocused_invalidate_due {
            if !self.owes_frames_regardless_of_focus()
                && Instant::now().saturating_duration_since(due) < Duration::from_millis(250)
            {
                return;
            }
        }

        // One tab lookup serves both the visibility check and the
        // full-rate probe below.
        let tab = Mux::get().get_active_tab_for_window(self.mux_window_id);
        let content_view_wants_output = self
            .active_content_view()
            .is_some_and(|view| view.wants_pane_output(pane_id));
        let visible = content_view_wants_output
            || match tab.as_ref() {
                Some(tab) => self.is_pane_visible_in_tab(tab, pane_id),
                None => false,
            };
        if !visible {
            return;
        }
        let Some(window) = self.window.clone() else {
            return;
        };

        // A decision is reached from here on, so a leftover latch — the
        // timer was swallowed, or the window entered an exempt state
        // with a timer armed — is resolved by this event; the in-flight
        // timer bails on its ownership check.
        self.unfocused_invalidate_due = None;

        // Throttling is only safe where the render watchdog can catch a
        // dropped frame; can_track_presented_terminal_output mirrors the
        // watchdog's own bail-outs (overview, fades, overlays, blocked
        // frontend surfaces), so those states stay at full rate.
        let unfocused_fps = self.config.unfocused_fps;
        let full_rate = self.focused.is_some()
            || unfocused_fps == 0
            || self.owes_frames_regardless_of_focus()
            || match tab.as_ref() {
                Some(tab) => !self.can_track_presented_terminal_output(tab),
                None => true,
            };

        let now = Instant::now();
        if full_rate || now >= self.unfocused_next_allowed {
            if !full_rate {
                self.unfocused_next_allowed =
                    now + Duration::from_millis(1000 / unfocused_fps.max(1));
            }
            window.invalidate();
            return;
        }

        // Trailing edge: arm one timer for the next allowed moment so the
        // last burst of output before a pane goes quiet still paints.
        let due = self.unfocused_next_allowed;
        self.unfocused_invalidate_due = Some(due);
        promise::spawn::spawn(async move {
            Timer::at(due).await;
            let win = window.clone();
            window.notify(TermWindowNotif::Apply(Box::new(move |tw| {
                // Only the timer that owns the current latch may act: a
                // focus cycle clears the latch and lets a newer timer
                // re-arm it; the stale timer must not clear that one's
                // claim or double the paint rate.
                if tw.unfocused_invalidate_due != Some(due) {
                    return;
                }
                tw.unfocused_invalidate_due = None;
                tw.unfocused_next_allowed =
                    Instant::now() + Duration::from_millis(1000 / tw.config.unfocused_fps.max(1));
                win.invalidate();
            })));
        })
        .detach();
    }

    fn mux_pane_output_event_callback(
        n: MuxNotification,
        window: &Window,
        mux_window_id: MuxWindowId,
        dead: &Arc<AtomicBool>,
    ) -> bool {
        if dead.load(Ordering::Relaxed) {
            // Subscription cancelled asynchronously
            return false;
        }

        match n {
            MuxNotification::Alert {
                pane_id,
                alert:
                    Alert::OutputSinceFocusLost
                    | Alert::CurrentWorkingDirectoryChanged
                    | Alert::WindowTitleChanged(_)
                    | Alert::TabTitleChanged(_)
                    | Alert::IconTitleChanged(_)
                    | Alert::Progress(_)
                    | Alert::SetUserVar { .. }
                    | Alert::Bell,
            }
            | MuxNotification::PaneFocused(pane_id)
            | MuxNotification::PaneRemoved(pane_id)
            | MuxNotification::AgentStatusChanged(pane_id)
            | MuxNotification::PaneOutput(pane_id) => {
                // Ideally we'd check to see if pane_id is part of this window,
                // but overlays may not be 100% associated with the window
                // in the mux and we don't want to lose the invalidation
                // signal for that case, so we just check window validity
                // here and propagate to the window event handler that
                // will then do the check with full context.
                let mux = Mux::get();
                if mux.get_window(mux_window_id).is_none() {
                    // Something inconsistent: cancel subscription
                    log::debug!(
                        "PaneOutput: wanted mux_window_id={} from mux, but \
                         was not found, cancel mux subscription",
                        mux_window_id
                    );
                    return false;
                }
                let _ = pane_id;
            }
            MuxNotification::PaneAdded(_pane_id) => {
                // If some other client spawns a pane inside this window, this
                // gives us an opportunity to attach it to the clipboard.
                let mux = Mux::get();
                return mux.get_window(mux_window_id).is_some();
            }
            MuxNotification::TabAddedToWindow { window_id, .. }
            | MuxNotification::WindowTitleChanged { window_id, .. }
            | MuxNotification::WindowInvalidated(window_id) => {
                if window_id != mux_window_id {
                    return true;
                }
            }
            MuxNotification::WindowRemoved(window_id) => {
                if window_id != mux_window_id {
                    return true;
                }
                // Set the window as dead to unsubscribe from further notifications
                dead.store(true, Ordering::Relaxed);
                return false;
            }
            MuxNotification::TabResized(tab_id)
            | MuxNotification::TabTitleChanged { tab_id, .. } => {
                let mux = Mux::get();
                if mux.window_containing_tab(tab_id) == Some(mux_window_id) {
                    // fall through
                } else {
                    return true;
                }
            }
            MuxNotification::FrontendLeaseChanged(ref state) => {
                let mux = Mux::get();
                if mux.window_containing_tab(state.tab_id) == Some(mux_window_id) {
                    // fall through
                } else {
                    return true;
                }
            }
            MuxNotification::FrontendAccessChanged(_) => {
                // Connection-wide: every window may need to replace terminal
                // contents with (or remove) the opaque handoff surface.
            }
            MuxNotification::Alert {
                alert: Alert::ToastNotification { .. },
                ..
            }
            | MuxNotification::AssignClipboard { .. }
            | MuxNotification::SaveToDownloads { .. }
            | MuxNotification::WindowCreated(_)
            | MuxNotification::ActiveWorkspaceChanged(_)
            | MuxNotification::WorkspaceRenamed { .. }
            | MuxNotification::Empty
            | MuxNotification::ThinkTermTreeChanged
            | MuxNotification::DefaultPaletteChanged
            | MuxNotification::ThinkTermSessionChanged
            | MuxNotification::WindowWorkspaceChanged(_) => return true,
            MuxNotification::Alert {
                alert: Alert::PaletteChanged { .. },
                ..
            } => {
                // fall through
            }
        }

        window.notify(TermWindowNotif::MuxNotification(n));

        true
    }

    fn subscribe_to_pane_updates(&self) {
        let window = self.window.clone().expect("window to be valid on startup");
        let mux_window_id = Arc::clone(&self.mux_window_id_for_subscriptions);
        let mux = Mux::get();
        let dead = Arc::new(AtomicBool::new(false));
        // Retire the previous subscription (it unregisters itself on its
        // next delivery) so re-subscribing is idempotent: callers may heal
        // a suspected-dead subscription without checking first.
        if let Some(prev) = self
            .pane_subscription_dead
            .borrow_mut()
            .replace(Arc::clone(&dead))
        {
            prev.store(true, Ordering::Relaxed);
        }
        mux.subscribe(move |n| {
            if dead.load(Ordering::Relaxed) {
                return false;
            }
            let mux_window_id = *mux_window_id.lock().unwrap();
            let window = window.clone();
            let dead = dead.clone();
            promise::spawn::spawn_into_main_thread(async move {
                Self::mux_pane_output_event_callback(n, &window, mux_window_id, &dead)
            })
            .detach();
            true
        });
    }

    fn emit_status_event(&mut self) {
        self.emit_window_event("update-right-status", None);
        self.emit_window_event("update-status", None);
    }

    fn invalidate_window_if(&self, should_invalidate: bool) {
        if should_invalidate {
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }
    }

    fn current_mux_workspace(&self) -> Option<String> {
        Mux::get()
            .get_window(self.mux_window_id)
            .map(|window| window.get_workspace().to_string())
    }

    fn sync_current_workspace_thread(&mut self) {
        let Some(workspace) = self.current_mux_workspace() else {
            return;
        };
        self.invalidate_window_if(crate::workspace_threads::sync_current_project(
            &self.active_space_id,
            &workspace,
        ));
    }

    fn refresh_thread_work_for_pane(&mut self, pane_id: PaneId) {
        self.invalidate_window_if(crate::workspace_threads::refresh_thread_work_for_pane(
            pane_id,
        ));
    }

    fn refresh_all_thread_work(&mut self) {
        self.invalidate_window_if(crate::workspace_threads::refresh_all_thread_work());
    }

    /// Move this window off a Space that no longer exists.
    ///
    /// A device connecting to a mux server for the first time has to choose a
    /// Space before it can authenticate, so it invents one; the server's real
    /// list only arrives afterwards and replaces it. `rehome` pairs each
    /// affected window with somewhere that does exist.
    pub(crate) fn rehome_if_space_vanished(&mut self, rehome: &[(u64, String)]) {
        let Some(target) = rehome
            .iter()
            .find(|(owner_id, _)| *owner_id == self.space_owner_id)
            .map(|(_, space_id)| space_id.clone())
        else {
            return;
        };
        if self.active_space_id == target {
            return;
        }
        log::info!(
            "Space {} is gone from its server; moving this window to {target}",
            self.active_space_id
        );
        if let Some(window) = self.window.clone() {
            self.switch_space(target, &window);
        }
    }

    pub(crate) fn switch_space(&mut self, space_id: String, window: &Window) {
        self.switch_space_to_thread(space_id, None, window);
    }

    /// Switch this window to another Space, activating `preferred_thread`
    /// when given (a notification jump) instead of the Space's recorded
    /// active thread — starting both activations can leave the window on the
    /// wrong thread when the first one materializes asynchronously.
    ///
    /// Returns false when no navigation happened (the destination Space is
    /// already shown in another window).
    pub(crate) fn switch_space_to_thread(
        &mut self,
        space_id: String,
        preferred_thread: Option<String>,
        window: &Window,
    ) -> bool {
        if self.workspace_sidebar_swipe.pending_switch_target() != Some(space_id.as_str()) {
            self.workspace_sidebar_swipe.cancel_immediately();
            self.clear_workspace_space_swipe_frame_transition();
        }
        if self.active_space_id == space_id {
            if let Some(thread_id) = preferred_thread {
                self.clear_right_sidebar_text_focus();
                self.activate_workspace_thread(thread_id, window);
            } else {
                window.invalidate();
            }
            return true;
        }
        self.snapshot_active_workspace_thread_layout();
        self.workspace_sidebar_pending_thread_selection = None;
        if !crate::workspace_threads::switch_window_space(self.space_owner_id, &space_id) {
            window.invalidate();
            return false;
        }
        let previous_sidebar_width = self.right_sidebar_width();
        // Text focus belongs to the Space being left. Clear it while that
        // Space is still active so Note state is frozen/saved against the
        // correct document and keyboard input can reach the destination pane.
        self.clear_right_sidebar_text_focus();
        self.set_content_view_active(false);
        self.remember_workspace_sidebar_scroll();
        self.active_space_id = space_id.clone();
        self.refresh_active_space_note_vault_flag();
        self.sync_content_view_surfaces_with_mux();
        self.workspace_sidebar_scroll_offset = self.remembered_workspace_sidebar_scroll();
        // Vault availability differs between Spaces; an expanded Note pane
        // can activate or deactivate here and the terminal must follow. The
        // reflow is deferred until switch_to_mux_window adopts the
        // destination (activation may attach or materialize asynchronously)
        // so the resize and its SIGWINCH reach the new Space's PTYs, not the
        // Space being left.
        self.pending_sidebar_reflow_width = Some(previous_sidebar_width);
        let target_thread = preferred_thread
            .or_else(|| crate::workspace_threads::ensure_active_thread_for_space(&space_id));
        if let Some(thread_id) = target_thread {
            self.activate_workspace_thread(thread_id, window);
        } else {
            // Nothing to adopt; settle the reflow immediately.
            self.consume_pending_sidebar_reflow();
            window.invalidate();
        }
        true
    }

    /// Delete a remote Space AND end its sessions on the server.  The shared
    /// tree deletion is acknowledged before any local mirror is removed, so
    /// the last window cannot detach the domain while its DeleteSpace RPC is
    /// still waiting to be polled.
    fn delete_space_and_remote_sessions(&mut self, space_id: &str) {
        self.start_delete_space(
            space_id,
            crate::workspace_threads::SpaceRemoval::Everywhere,
            true,
        );
    }

    fn delete_space(
        &mut self,
        space_id: &str,
        _window: Option<&Window>,
        removal: crate::workspace_threads::SpaceRemoval,
    ) {
        self.start_delete_space(space_id, removal, false);
    }

    /// Archive a project off the sidebar: snapshot the on-screen layout,
    /// close the project's panes (remote ones die on the server first), and
    /// land the window on the next live project. Async because the remote
    /// path must await pane kills and the server's verdict.
    pub(crate) fn start_archive_project(&mut self, project_id: &str) {
        // The workspace currently on screen deserves a fresh snapshot so
        // unarchiving restores what the user last saw, not an older save.
        self.snapshot_active_workspace_thread_layout();
        let project_id = project_id.to_string();
        let gui_window = self.window.clone();
        promise::spawn::spawn_into_main_thread(async move {
            let result = crate::workspace_threads::archive_project(&project_id).await;
            if let Some(gui_window) = gui_window {
                gui_window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    term_window.finish_archive_project(result);
                })));
            }
        })
        .detach();
    }

    fn finish_archive_project(
        &mut self,
        result: Result<
            crate::workspace_threads::ArchivedProject,
            crate::workspace_threads::ArchiveProjectError,
        >,
    ) {
        let window = self.window.clone();
        match result {
            Ok(archived) => {
                let cleanup_workspaces = archived.materialized_workspace_names;
                if archived.was_active {
                    if let (Some(next_thread_id), Some(window)) =
                        (archived.next_thread_id.clone(), window.as_ref())
                    {
                        // The successor may still need materializing, in
                        // which case adoption is deferred -- killing the
                        // workspaces synchronously here would take down this
                        // window's own mux window. Route the cleanup through
                        // the activation, like finish_removed_project does.
                        self.activate_workspace_thread_with_cleanup(
                            next_thread_id,
                            window,
                            cleanup_workspaces,
                        );
                        return;
                    }
                }
                // Local mirrors of the archived workspaces; remote panes are
                // already gone by the time the store call returns, so any
                // mirror still present must not send a duplicate KillPane
                // while its window is torn down.
                let mux = Mux::get();
                for workspace in cleanup_workspaces {
                    for window_id in mux.iter_windows_in_workspace(&workspace) {
                        let panes = mux
                            .get_window(window_id)
                            .map(|window| {
                                window
                                    .iter()
                                    .flat_map(|tab| tab.iter_all_panes())
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        for pane in panes {
                            if let Some(client) =
                                pane.downcast_ref::<wezterm_client::pane::ClientPane>()
                            {
                                client.ignore_next_kill();
                            }
                        }
                        mux.kill_window(window_id);
                    }
                }
            }
            Err(err) => {
                let text = match err {
                    crate::workspace_threads::ArchiveProjectError::LastLiveProject => {
                        crate::i18n::tr("archive-project-last-live-project")
                    }
                    crate::workspace_threads::ArchiveProjectError::ServerRejected => {
                        crate::i18n::tr("archive-project-server-rejected")
                    }
                    crate::workspace_threads::ArchiveProjectError::TeardownIncomplete => {
                        crate::i18n::tr("archive-project-teardown-incomplete")
                    }
                    // RemoteUnavailable already raised its own toast via
                    // notify_remote_tree_mutation_unavailable; the rest are
                    // benign races (row already gone / already archived).
                    _ => {
                        if let Some(window) = window.as_ref() {
                            window.invalidate();
                        }
                        return;
                    }
                };
                log::error!("archive project failed: {text}");
                wezterm_toast_notification::persistent_toast_notification("ThinkTerm", &text);
            }
        }
        if let Some(window) = window.as_ref() {
            window.invalidate();
        }
    }

    pub(crate) fn start_delete_space(
        &mut self,
        space_id: &str,
        removal: crate::workspace_threads::SpaceRemoval,
        end_remote_sessions: bool,
    ) {
        let owner_id = self.space_owner_id;
        let space_id = space_id.to_string();
        let gui_window = self.window.clone();
        promise::spawn::spawn_into_main_thread(async move {
            let result = crate::workspace_threads::delete_space_for_window(
                owner_id,
                &space_id,
                removal,
                end_remote_sessions,
            )
            .await;
            if let Some(gui_window) = gui_window {
                gui_window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    term_window.finish_delete_space(&space_id, result);
                })));
            }
        })
        .detach();
    }

    fn finish_delete_space(
        &mut self,
        space_id: &str,
        result: Result<
            crate::workspace_threads::DeletedSpace,
            crate::workspace_threads::DeleteSpaceError,
        >,
    ) {
        let window = self.window.clone();
        match result {
            Ok(deleted) => {
                let deleted_active_space = self.active_space_id == space_id;
                if deleted_active_space {
                    let previous_sidebar_width = self.right_sidebar_width();
                    self.workspace_sidebar_scroll_offsets.remove(space_id);
                    self.active_space_id = deleted.fallback_space_id.clone();
                    self.refresh_active_space_note_vault_flag();
                    self.sync_content_view_surfaces_with_mux();
                    self.workspace_sidebar_scroll_offset =
                        self.remembered_workspace_sidebar_scroll();
                    // Deferred until the fallback Space's mux window is
                    // adopted so the resize does not hit the deleted Space's
                    // panes.
                    self.pending_sidebar_reflow_width = Some(previous_sidebar_width);
                    if let Some(window) = window.as_ref() {
                        if let Some(thread_id) =
                            crate::workspace_threads::ensure_active_thread_for_space(
                                &self.active_space_id,
                            )
                        {
                            self.activate_workspace_thread(thread_id, window);
                        } else {
                            self.consume_pending_sidebar_reflow();
                            window.invalidate();
                        }
                    } else {
                        self.consume_pending_sidebar_reflow();
                    }
                }

                // Mirror teardown happens only after the server tree has
                // acknowledged the deletion.  Normal server/local disconnect
                // preserves sessions; the explicit end-sessions action sends
                // remote kills, still scoped to this Space's workspaces.
                let mux = Mux::get();
                for workspace in deleted.materialized_workspace_names {
                    for window_id in mux.iter_windows_in_workspace(&workspace) {
                        let panes = mux
                            .get_window(window_id)
                            .map(|window| {
                                window
                                    .iter()
                                    .flat_map(|tab| tab.iter_all_panes())
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        for pane in panes {
                            if let Some(client) =
                                pane.downcast_ref::<wezterm_client::pane::ClientPane>()
                            {
                                // Explicit end-session deletions already used
                                // the awaited KillPane path before the server
                                // tree was removed. Any mirrors still present
                                // here must not send a duplicate request while
                                // the window is torn down.
                                client.ignore_next_kill();
                            }
                        }
                        mux.kill_window(window_id);
                    }
                }
                if let Some(window) = window {
                    window.invalidate();
                }
            }
            Err(err) => {
                log::warn!("failed to delete ThinkTerm space {space_id}: {err:?}");
                if let Some(window) = window {
                    window.invalidate();
                }
            }
        }
    }

    fn schedule_window_event(&mut self, name: &str, pane_id: Option<PaneId>) {
        let window = GuiWin::new(self);
        let pane = match pane_id {
            Some(pane_id) => Mux::get().get_pane(pane_id),
            None => None,
        };
        let pane = match pane {
            Some(pane) => pane,
            None => match self.get_active_pane_or_overlay() {
                Some(pane) => pane,
                None => return,
            },
        };
        let pane = MuxPane(pane.pane_id());
        let name = name.to_string();

        async fn do_event(
            lua: Option<Rc<mlua::Lua>>,
            name: String,
            window: GuiWin,
            pane: MuxPane,
        ) -> anyhow::Result<()> {
            let again = if let Some(lua) = lua {
                let args = lua.pack_multi((window.clone(), pane))?;

                if let Err(err) = config::lua::emit_event(&lua, (name.clone(), args)).await {
                    log::error!("while processing {} event: {:#}", name, err);
                }
                true
            } else {
                false
            };

            window
                .window
                .notify(TermWindowNotif::FinishWindowEvent { name, again });

            Ok(())
        }

        promise::spawn::spawn(config::with_lua_config_on_main_thread(move |lua| {
            do_event(lua, name, window, pane)
        }))
        .detach();
    }

    /// Called as part of finishing up a callout to lua.
    /// If again==false it means that there isn't a lua config
    /// to execute against, so we should just mark as done.
    /// Otherwise, if there is a queued item, schedule it now.
    fn finish_window_event(&mut self, name: &str, again: bool) {
        let state = self
            .event_states
            .entry(name.to_string())
            .or_insert(EventState::None);
        if again {
            match state {
                EventState::InProgress => {
                    *state = EventState::None;
                }
                EventState::InProgressWithQueued(pane) => {
                    let pane = *pane;
                    *state = EventState::InProgress;
                    self.schedule_window_event(name, pane);
                }
                EventState::None => {}
            }
        } else {
            *state = EventState::None;
        }
    }

    pub fn emit_window_event(&mut self, name: &str, pane_id: Option<PaneId>) {
        if self.get_active_pane_or_overlay().is_none() || self.window.is_none() {
            return;
        }

        let state = self
            .event_states
            .entry(name.to_string())
            .or_insert(EventState::None);
        match state {
            EventState::InProgress => {
                // Flag that we want to run again when the currently
                // executing event calls finish_window_event().
                *state = EventState::InProgressWithQueued(pane_id);
                return;
            }
            EventState::InProgressWithQueued(other_pane) => {
                // We've already got one copy executing and another
                // pending dispatch, so don't queue another.
                if pane_id != *other_pane {
                    log::warn!(
                        "Cannot queue {} event for pane {:?}, as \
                         there is already an event queued for pane {:?} \
                         in the same window",
                        name,
                        pane_id,
                        other_pane
                    );
                }
                return;
            }
            EventState::None => {
                // Nothing pending, so schedule a call now
                *state = EventState::InProgress;
                self.schedule_window_event(name, pane_id);
            }
        }
    }

    fn check_for_dirty_lines_and_invalidate_selection(&mut self, pane: &Arc<dyn Pane>) {
        let dims = pane.get_dimensions();
        let viewport = self
            .get_viewport(pane.pane_id())
            .unwrap_or(dims.physical_top);
        let visible_range = viewport..viewport + dims.viewport_rows as StableRowIndex;
        let seqno = self.selection(pane.pane_id()).seqno;
        let dirty = pane.get_changed_since(visible_range, seqno);

        if dirty.is_empty() {
            return;
        }
        if pane.downcast_ref::<CopyOverlay>().is_none()
            && pane.downcast_ref::<QuickSelectOverlay>().is_none()
        {
            // If any of the changed lines intersect with the
            // selection, then we need to clear the selection, but not
            // when the search overlay is active; the search overlay
            // marks lines as dirty to force invalidate them for
            // highlighting purpose but also manipulates the selection
            // and we want to allow it to retain the selection it made!

            let clear_selection =
                if let Some(selection_range) = self.selection(pane.pane_id()).range.as_ref() {
                    let selection_rows = selection_range.rows();
                    selection_rows.into_iter().any(|row| dirty.contains(row))
                } else {
                    false
                };

            if clear_selection {
                self.selection(pane.pane_id()).range.take();
                self.selection(pane.pane_id()).origin.take();
                self.selection(pane.pane_id()).seqno = pane.get_current_seqno();
            }
        }
    }
}

impl TermWindow {
    /// The chrome's colours: the sidebars, the tab bar, the pane nav bars and
    /// every other surface this window paints around the terminal.
    ///
    /// Resolved in `config_was_reloaded` rather than here, so a draw costs a
    /// copy of a `Copy` struct. Distinct from [`Self::palette`] below, which is
    /// the *terminal's* colours.
    pub(crate) fn chrome(&self) -> crate::ui::UiPalette {
        self.chrome_palette
    }

    /// Re-resolve the contrast floor and retire the quads that were built
    /// under the old one. `config_was_reloaded` re-resolves it too; this is
    /// the direct route, for the settings window, which writes the choice to
    /// its own file and never touches the configuration.
    pub(crate) fn refresh_text_min_contrast(&mut self) {
        let next = crate::native_settings::text_min_contrast_ratio(&self.config);
        if next == self.text_min_contrast {
            return;
        }
        self.text_min_contrast = next;
        // Colours are baked into the quad and shape cache *values* while the
        // keys carry only this generation, so bumping it is what retires
        // them. The memo below is keyed on the ratio and so cannot go stale.
        self.shape_generation += 1;
        self.shape_cache.borrow_mut().clear();
        self.invalidate_window();
    }

    /// Re-resolve the cached chrome colours. `config_was_reloaded` does this
    /// too; this is the direct route, for when the appearance moved without a
    /// configuration reload behind it.
    pub(crate) fn refresh_chrome(&mut self) {
        self.chrome_palette = crate::native_settings::chrome_palette(
            crate::native_settings::load_shared().appearance.theme_mode,
            crate::native_settings::effective_appearance(),
            &self.config,
            self.scheme_preview_ground,
        );
        self.invalidate_window();
    }

    fn palette(&mut self) -> &ColorPalette {
        if self.palette.is_none() {
            self.palette
                .replace(config::TermConfig::new().color_palette());
        }
        self.palette.as_ref().unwrap()
    }

    /// Apply (or clear) a color-scheme override on this window without
    /// touching persistence — the same mechanism as
    /// `window:set_config_overrides`. A no-op when nothing changes.
    /// True when the overrides moved, which means `config_was_reloaded` has
    /// already run. Callers about to reload anyway can use that to not do it
    /// twice -- a reload re-runs the Lua configuration from disk.
    pub(crate) fn apply_color_scheme_override(&mut self, name: Option<String>) -> bool {
        use wezterm_dynamic::{ToDynamic, Value};
        let mut map = match &self.config_overrides {
            Value::Object(obj) => obj.clone(),
            _ => Default::default(),
        };
        let key = "color_scheme".to_dynamic();
        match &name {
            Some(scheme) => {
                map.insert(key, scheme.to_dynamic());
            }
            None => {
                map.remove(&key);
            }
        }
        let next = Value::Object(map);
        if next == self.config_overrides
            || (matches!(&next, Value::Object(obj) if obj.is_empty())
                && matches!(&self.config_overrides, Value::Null))
        {
            return false;
        }
        self.config_overrides = next;
        self.config_was_reloaded();
        true
    }

    /// The command palette's theme switch: apply to this window, persist the
    /// choice for new windows and the next launch, and let every other open
    /// window follow. The broadcast reaches this window too; the second
    /// application no-ops on the unchanged overrides.
    fn set_color_scheme_override(&mut self, name: Option<String>) {
        // Persisted *before* the override is applied. Applying it reloads the
        // configuration, which re-resolves the chrome, which under "follow
        // terminal colours" reads the chosen scheme back out of the settings.
        // Saving second meant that read saw the previous scheme: the interface
        // flashed the colours it was leaving before arriving at the ones it
        // was asked for.
        crate::native_settings::save_color_scheme(name.clone());
        // A scheme can move which side the interface is on, and the platform's
        // own chrome takes its tint from that.
        crate::native_settings::apply_preferred_appearance(
            crate::native_settings::load_shared().appearance.theme_mode,
        );
        self.apply_color_scheme_override(name.clone());
        // `apply_color_scheme_override` is a no-op when this window's
        // overrides already name the scheme -- which is exactly the case after
        // a preview -- so the chrome is refreshed here rather than relying on
        // the reload that call may not perform.
        self.refresh_chrome();
        // The settings window derives its own surfaces from this scheme too,
        // and is in no broadcast list of its own.
        crate::settings_window::refresh_open_settings_window_chrome();
        if let Some(front_end) = crate::frontend::try_front_end() {
            for gui_window in front_end.gui_windows() {
                let name = name.clone();
                gui_window
                    .window
                    .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                        term_window.apply_color_scheme_override(name);
                    })));
            }
        }
    }

    /// This window's terminal configuration, as `config_was_reloaded` hands
    /// it to every pane: the global configuration with this window's
    /// `config_overrides` -- the colour scheme picked in the palette or the
    /// settings window -- applied.
    fn window_term_config(&self) -> Arc<dyn TerminalConfiguration> {
        Arc::new(TermConfig::with_config(self.config.clone()))
    }

    /// Hand `tab`'s panes this window's configuration.
    ///
    /// A pane this window spawns gets it from `spawn_command_impl`. A pane
    /// that arrives any other way -- attached from the local session server
    /// when the window opens, restored with a thread, spawned by another
    /// client -- is born with the *global* configuration (`ClientPane::new`
    /// reads `configuration()`), which knows nothing about the scheme in
    /// this window's `config_overrides`. `config_was_reloaded` pushes the
    /// window's configuration to every pane it has, but only when the
    /// configuration changes; nothing pushed it to a pane that turned up in
    /// between, so a mux-backed pane painted the file's colours until the
    /// next reload. Pushing again to a pane that already has it is idle:
    /// `ClientPane::set_config` skips the server round trip for a palette
    /// the server already holds.
    fn push_window_config_to_tab(&self, tab_id: TabId) {
        let mux = Mux::get();
        let Some(window) = mux.get_window(self.mux_window_id) else {
            return;
        };
        let Some(tab) = window.iter().find(|tab| tab.tab_id() == tab_id) else {
            return;
        };
        let term_config = self.window_term_config();
        for pos in tab.iter_panes_ignoring_zoom() {
            log::debug!(
                "pushing window config to pane {} of tab {tab_id} (window {})",
                pos.pane.pane_id(),
                self.mux_window_id
            );
            pos.pane.set_config(Arc::clone(&term_config));
        }
    }

    /// The single-pane form of [`Self::push_window_config_to_tab`], for a
    /// pane added to a tab this window already holds. A pane whose tab is not
    /// in this window yet is left alone: `TabAddedToWindow` covers it.
    fn push_window_config_to_pane(&self, pane_id: PaneId) {
        let mux = Mux::get();
        let Some(window) = mux.get_window(self.mux_window_id) else {
            return;
        };
        for tab in window.iter() {
            if let Some(pos) = tab
                .iter_panes_ignoring_zoom()
                .into_iter()
                .find(|pos| pos.pane.pane_id() == pane_id)
            {
                log::debug!(
                    "pushing window config to added pane {pane_id} (window {})",
                    self.mux_window_id
                );
                pos.pane.set_config(self.window_term_config());
                return;
            }
        }
    }

    /// Every tab this window holds; for a window opened over content that
    /// was attached before it existed.
    pub(crate) fn push_window_config_to_all_tabs(&self) {
        let mux = Mux::get();
        let Some(window) = mux.get_window(self.mux_window_id) else {
            return;
        };
        let tab_ids: Vec<TabId> = window.iter().map(|tab| tab.tab_id()).collect();
        for tab_id in tab_ids {
            self.push_window_config_to_tab(tab_id);
        }
    }

    pub fn config_was_reloaded(&mut self) {
        log::debug!(
            "config was reloaded, overrides: {:?}",
            self.config_overrides
        );
        self.key_table_state.clear_stack();
        self.connection_name = Connection::get().unwrap().name();
        let config = match config::overridden_config(&self.config_overrides) {
            Ok(config) => config,
            Err(err) => {
                log::error!(
                    "Failed to apply config overrides to window: {:#}: {:?}",
                    err,
                    self.config_overrides
                );
                configuration()
            }
        };
        self.config = config.clone();
        self.palette.take();
        // One place for all three ways the chrome's colours can move: a
        // configuration reload, a colour scheme override, and an appearance
        // change (which routes here too).
        //
        // Above the `mux.get_window` bail below, not after it: a window that
        // outlives its mux window still paints, and nothing else re-resolves
        // these two -- the settings-window routes reach them only through
        // their own notifies.
        self.chrome_palette = crate::native_settings::chrome_palette(
            crate::native_settings::load_shared().appearance.theme_mode,
            crate::native_settings::effective_appearance(),
            &config,
            self.scheme_preview_ground,
        );
        self.text_min_contrast = crate::native_settings::text_min_contrast_ratio(&config);
        // Under "follow terminal colours" a scheme changed in the
        // configuration file moves which side the interface is on, and the
        // platform's own chrome -- title bar, native buttons, menus -- takes
        // its tint from the connection's appearance, which only
        // `apply_preferred_appearance` moves. The settings window and the
        // command palette call it on their own routes; this is the file's.
        // Guarded on an actual change because setting it feeds an appearance
        // event back through here.
        {
            let mode = crate::native_settings::load_shared().appearance.theme_mode;
            if mode == crate::native_settings::NativeThemeMode::FollowTerminal
                && self.chrome_palette.appearance
                    != crate::native_settings::effective_appearance()
            {
                crate::native_settings::apply_preferred_appearance(mode);
            }
        }

        let mux = Mux::get();
        let window = match mux.get_window(self.mux_window_id) {
            Some(window) => window,
            _ => return,
        };
        if window.len() == 1 {
            self.show_tab_bar = config.enable_tab_bar && !config.hide_tab_bar_if_only_one_tab;
        } else {
            self.show_tab_bar = config.enable_tab_bar;
        }
        *self.cursor_blink_state.borrow_mut() = ColorEase::new(
            config.cursor_blink_rate,
            config.cursor_blink_ease_in,
            config.cursor_blink_rate,
            config.cursor_blink_ease_out,
            None,
        );
        *self.blink_state.borrow_mut() = ColorEase::new(
            config.text_blink_rate,
            config.text_blink_ease_in,
            config.text_blink_rate,
            config.text_blink_ease_out,
            None,
        );
        *self.rapid_blink_state.borrow_mut() = ColorEase::new(
            config.text_blink_rate_rapid,
            config.text_blink_rapid_ease_in,
            config.text_blink_rate_rapid,
            config.text_blink_rapid_ease_out,
            None,
        );

        self.show_scroll_bar = config.enable_scroll_bar;
        self.shape_generation += 1;
        {
            let mut shape_cache = self.shape_cache.borrow_mut();
            shape_cache.update_config(&config);
            shape_cache.clear();
        }
        {
            let mut ui_shape_caches = self.ui_shape_caches.borrow_mut();
            ui_shape_caches.update_config(&config);
            ui_shape_caches.clear_all();
        }
        self.publish_ui_shape_cache_diagnostics();
        self.line_state_cache.borrow_mut().update_config(&config);
        self.line_quad_cache.borrow_mut().update_config(&config);
        self.line_to_ele_shape_cache
            .borrow_mut()
            .update_config(&config);
        self.pane_font_cache.borrow_mut().clear();
        // The held scales name entries in the cache just emptied, and a config
        // change can move the font size out from under them anyway.
        self.preview_scale_hold.borrow_mut().clear();
        self.fancy_tab_bar.take();
        self.invalidate_fancy_tab_bar();
        self.invalidate_modal();
        // The command list, key labels and fonts all just changed out from
        // under it; reopen rather than repair.
        self.close_command_palette();
        self.input_map = InputMap::new(&config);
        self.leader_is_down = None;
        self.render_state.as_mut().map(|rs| rs.config_changed());
        let dimensions = self.dimensions;

        if let Err(err) = self.fonts.config_changed(&config) {
            log::error!("Failed to load font configuration: {:#}", err);
        }

        if let Some(window) = mux.get_window(self.mux_window_id) {
            let term_config: Arc<dyn TerminalConfiguration> =
                Arc::new(TermConfig::with_config(config.clone()));
            for tab in window.iter() {
                for pane in tab.iter_panes_ignoring_zoom() {
                    pane.pane.set_config(Arc::clone(&term_config));
                }
            }
            for state in self.pane_state.borrow().values() {
                if let Some(overlay) = &state.overlay {
                    overlay.pane.set_config(Arc::clone(&term_config));
                }
            }
            for state in self.tab_state.borrow().values() {
                if let Some(overlay) = &state.overlay {
                    overlay.pane.set_config(Arc::clone(&term_config));
                }
            }
        }

        if let Some(window) = self.window.as_ref().map(|w| w.clone()) {
            self.load_os_parameters();
            self.apply_scale_change(&dimensions, self.fonts.get_font_scale());
            self.apply_dimensions(&dimensions, None, &window);
            // Config reload may also restore a persisted per-pane font scale.
            // That changes the pane's rows/cols even when the window's overall
            // TerminalSize is unchanged, so publish a complete viewport rather
            // than limiting the update to the local render surface.
            if self.stage_workspace_thread_font_scales() {
                self.sync_active_tab_geometry_now();
            }
            window.config_did_change(&config);
            window.invalidate();
        }

        // Do this after we've potentially adjusted scaling based on config/padding
        // and window size
        self.window_background = reload_background_image(
            &config,
            &self.window_background,
            &self.dimensions,
            &self.render_metrics,
        );

        self.invalidate_modal();
        self.emit_window_event("window-config-reloaded", None);
    }

    /// True when a content view is the foreground content (occupying the
    /// content area instead of terminal panes).
    pub(crate) fn content_view_foreground(&self) -> bool {
        self.active_content_view_index().is_some()
    }

    /// Whether a full-window view is arriving or leaving right now, and so
    /// owns the keyboard and the pointer.
    ///
    /// Every other input gate asks [`Self::content_view_foreground`], which is
    /// false for the whole of a *closing* transition: the view is removed from
    /// `content_views` before the fade is started. Without this the terminal
    /// was reachable for the length of that animation, while what the user was
    /// looking at was a recording of it on its way into a card.
    pub(crate) fn content_view_transition_running(&self) -> bool {
        let now = Instant::now();
        self.content_view_fade.as_ref().is_some_and(|fade| {
            crate::termwindow::content_view::transition_holds_input(
                now.saturating_duration_since(fade.started_at),
            )
        })
    }

    pub(crate) fn active_content_view_presentation(&self) -> ContentViewPresentation {
        self.active_content_view()
            .map(|view| view.presentation())
            .unwrap_or_default()
    }

    pub(crate) fn content_view_is_full_window(&self) -> bool {
        self.content_view_foreground()
            && self.active_content_view_presentation() == ContentViewPresentation::FullWindow
    }

    pub(crate) fn full_window_client_chrome_height(&self) -> f32 {
        if crate::termwindow::ui::platform_chrome::full_window_needs_client_chrome(
            self.config.window_decorations,
            self.window_state,
            cfg!(target_os = "macos"),
        ) {
            self.tab_bar_pixel_height().unwrap_or(0.0)
        } else {
            0.0
        }
    }

    pub(crate) fn content_view_area(&self) -> RectF {
        let border = self.get_os_border();
        if self.content_view_is_full_window() {
            let left = border.left.get() as f32;
            // Full-window views suppress the app tab strip, not the platform's
            // essential controls. Reserve the same row independently of
            // enable_tab_bar/hide_tab_bar_if_only_one_tab.
            let top = border.top.get() as f32 + self.full_window_client_chrome_height();
            let right = self
                .dimensions
                .pixel_width
                .saturating_sub(border.right.get() as usize) as f32;
            let bottom = self
                .dimensions
                .pixel_height
                .saturating_sub(border.bottom.get() as usize) as f32;
            return euclid::rect(left, top, (right - left).max(0.0), (bottom - top).max(0.0));
        }
        let top_tab_h = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height().unwrap_or(0.0)
        } else {
            0.0
        };
        let bottom_tab_h = if self.show_tab_bar && self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height().unwrap_or(0.0)
        } else {
            0.0
        };
        // Deliberately *not* inset by `window_padding`: that padding belongs to
        // the terminal grid, and content views are UI panels that own the whole
        // area. Applying it only added a left/top gap (the right/bottom edges
        // never subtracted it) that nothing painted, so the window background
        // showed through as an L-shaped border.
        let left = self.workspace_sidebar_width() as f32 + border.left.get() as f32;
        let top = border.top.get() as f32 + top_tab_h;
        let right = self
            .dimensions
            .pixel_width
            .saturating_sub(border.right.get() as usize)
            .saturating_sub(self.right_sidebar_width()) as f32;
        let bottom =
            (self.dimensions.pixel_height as f32 - border.bottom.get() as f32 - bottom_tab_h)
                .max(top);
        euclid::rect(left, top, (right - left).max(0.0), (bottom - top).max(0.0))
    }

    /// Where the terminal grid itself lives: the window minus the sidebars,
    /// the tab bar and the borders.
    ///
    /// Unlike [`Self::content_view_area`] this ignores whether a full-window
    /// view is currently open, because it answers a question about the
    /// terminal rather than about the view sitting on top of it.
    pub(crate) fn terminal_content_rect(&self) -> RectF {
        let border = self.get_os_border();
        let top_tab_h = if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height().unwrap_or(0.0)
        } else {
            0.0
        };
        let bottom_tab_h = if self.show_tab_bar && self.config.tab_bar_at_bottom {
            self.tab_bar_pixel_height().unwrap_or(0.0)
        } else {
            0.0
        };
        let left = self.workspace_sidebar_width() as f32 + border.left.get() as f32;
        let top = border.top.get() as f32 + top_tab_h;
        let right = self
            .dimensions
            .pixel_width
            .saturating_sub(border.right.get() as usize)
            .saturating_sub(self.right_sidebar_width()) as f32;
        let bottom =
            (self.dimensions.pixel_height as f32 - border.bottom.get() as f32 - bottom_tab_h)
                .max(top);
        euclid::rect(left, top, (right - left).max(0.0), (bottom - top).max(0.0))
    }

    fn content_view_visible_in_active_space(&self, tab: &ContentViewTab) -> bool {
        match tab.space_id.as_deref() {
            Some(space_id) => space_id == self.active_space_id,
            None => true,
        }
    }

    fn content_view_shown_in_tab_bar(&self, tab: &ContentViewTab) -> bool {
        self.content_view_visible_in_active_space(tab) && tab.view.show_in_tab_bar()
    }

    fn active_content_view_shown_in_tab_bar(&self) -> bool {
        self.active_content_view_id.is_some_and(|active_id| {
            self.content_views
                .iter()
                .any(|tab| tab.id == active_id && self.content_view_shown_in_tab_bar(tab))
        })
    }

    fn content_view_surface_id(id: ContentViewId) -> String {
        format!("thinkterm-content-view:{id}")
    }

    fn sync_content_view_surfaces_with_mux(&mut self) {
        let mux = Mux::get();
        let mux_window_id = self.mux_window_id;
        let foreground_ids = self
            .active_content_view_id
            .filter(|active_id| {
                self.content_views.iter().any(|tab| {
                    tab.id == *active_id && self.content_view_visible_in_active_space(tab)
                })
            })
            .into_iter()
            .collect::<HashSet<_>>();

        let registered = self
            .registered_content_view_surfaces
            .iter()
            .map(|(id, window_id)| (*id, *window_id))
            .collect::<Vec<_>>();
        for (id, window_id) in registered {
            if window_id != mux_window_id || !foreground_ids.contains(&id) {
                mux.unregister_window_ui_surface(window_id, &Self::content_view_surface_id(id));
                self.registered_content_view_surfaces.remove(&id);
            }
        }

        for id in foreground_ids {
            if self.registered_content_view_surfaces.get(&id).copied() == Some(mux_window_id) {
                continue;
            }

            if mux.register_window_ui_surface(mux_window_id, Self::content_view_surface_id(id)) {
                self.registered_content_view_surfaces
                    .insert(id, mux_window_id);
            }
        }
    }

    fn active_content_view_index(&self) -> Option<usize> {
        let active_id = self.active_content_view_id?;
        self.content_views
            .iter()
            .position(|tab| tab.id == active_id && self.content_view_visible_in_active_space(tab))
    }

    pub(crate) fn active_content_view(&self) -> Option<&dyn ContentView> {
        self.active_content_view_index()
            .map(|idx| self.content_views[idx].view.as_ref())
    }

    pub(crate) fn active_content_view_mut(&mut self) -> Option<&mut dyn ContentView> {
        let idx = self.active_content_view_index()?;
        Some(self.content_views[idx].view.as_mut())
    }

    fn content_view_mut_by_id(&mut self, id: ContentViewId) -> Option<&mut dyn ContentView> {
        let idx = self.content_views.iter().position(|tab| tab.id == id)?;
        Some(self.content_views[idx].view.as_mut())
    }

    fn content_view_id_for_key(&self, key: &str) -> Option<ContentViewId> {
        self.content_views
            .iter()
            .find(|tab| {
                tab.key.as_deref() == Some(key) && self.content_view_visible_in_active_space(tab)
            })
            .map(|tab| tab.id)
    }

    fn active_content_view_key_is(&self, key: &str) -> bool {
        self.active_content_view_id
            .and_then(|id| self.content_views.iter().find(|tab| tab.id == id))
            .and_then(|tab| tab.key.as_deref())
            == Some(key)
    }

    pub(crate) fn active_content_view_is_remote_thread(&self) -> bool {
        self.active_content_view()
            .and_then(|view| view.tab_key())
            .is_some_and(|key| {
                key.starts_with(
                    crate::termwindow::remote_thread_view::REMOTE_THREAD_CONTENT_VIEW_KEY_PREFIX,
                )
            })
    }

    pub(crate) fn content_view_count(&self) -> usize {
        self.content_views
            .iter()
            .filter(|tab| self.content_view_shown_in_tab_bar(tab))
            .count()
    }

    fn content_view_pending_thread_selection(&self, id: ContentViewId) -> Option<String> {
        self.content_views
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.key.as_deref())
            .and_then(|key| {
                key.strip_prefix(
                    crate::termwindow::remote_thread_view::REMOTE_THREAD_CONTENT_VIEW_KEY_PREFIX,
                )
            })
            .map(ToString::to_string)
    }

    fn sync_workspace_sidebar_pending_thread_selection(&mut self) {
        self.workspace_sidebar_pending_thread_selection = self
            .active_content_view_id
            .and_then(|id| self.content_view_pending_thread_selection(id));
    }

    fn set_active_content_view_id(&mut self, id: Option<ContentViewId>) {
        let was_foreground = self.content_view_foreground();
        self.active_content_view_id = id.filter(|id| {
            self.content_views
                .iter()
                .any(|tab| tab.id == *id && self.content_view_visible_in_active_space(tab))
        });
        if self.content_view_is_full_window() {
            self.workspace_sidebar_swipe.cancel_immediately();
            self.clear_workspace_space_swipe_frame_transition();
        }
        self.sync_content_view_surfaces_with_mux();
        self.sync_workspace_sidebar_pending_thread_selection();
        let is_foreground = self.content_view_foreground();
        if crate::termwindow::content_view::take_deferred_mux_resize_on_exit(
            &mut self.content_view_deferred_mux_resize,
            was_foreground,
            is_foreground,
        ) {
            self.resize_mux_tabs_to_current_terminal_size();
        }
        self.invalidate_window();
    }

    fn reactivate_content_view_by_id(&mut self, id: ContentViewId) {
        let response = self
            .content_view_mut_by_id(id)
            .map(|view| view.on_reactivated())
            .unwrap_or(crate::termwindow::content_view::ContentViewResponse::Ignored);
        self.handle_content_response_for(id, response);
    }

    pub(crate) fn open_content_view(&mut self, view: Box<dyn ContentView>) -> ContentViewId {
        let key = view.tab_key();
        let space_id = view.space_id().map(ToString::to_string);
        if let Some(existing_id) = key
            .as_deref()
            .and_then(|key| self.content_view_id_for_key(key))
        {
            self.set_active_content_view_id(Some(existing_id));
            // A reactivation response may close the tab; callers only use this
            // as an activation request and do not rely on the returned id
            // remaining open.
            self.reactivate_content_view_by_id(existing_id);
            return existing_id;
        }

        let id = self.next_content_view_id;
        self.next_content_view_id = self.next_content_view_id.saturating_add(1).max(1);
        let full_window = matches!(view.presentation(), ContentViewPresentation::FullWindow);
        self.content_views.push(ContentViewTab {
            id,
            key,
            space_id,
            view,
        });
        self.set_active_content_view_id(Some(id));
        if full_window {
            self.begin_content_view_fade(false);
        }
        id
    }

    /// Start a full-window view's arrival or departure.
    ///
    /// The clock is deliberately left unstarted: the first frame of an
    /// arriving view rasterises every glyph it contains and grows the atlas
    /// for them, and charging that to the transition would spend most of it
    /// on a picture nobody has seen yet.
    fn begin_content_view_fade(&mut self, closing: bool) {
        self.begin_content_view_fade_to(closing, None);
    }

    fn begin_content_view_fade_to(&mut self, closing: bool, destination: Option<RectF>) {
        let now = Instant::now();
        let ghost = closing
            .then(|| self.content_view_last_frame.take())
            .flatten();
        if closing && ghost.is_none() {
            // Nothing was ever composited for this view -- it opened and
            // closed inside a single frame. There is no picture to take away.
            self.content_view_fade = None;
            return;
        }
        let (from, to) = if closing { (1.0, 0.0) } else { (0.0, 1.0) };
        // A toggle can land while the previous one is still running. Departing
        // from where each value currently is, rather than from the far end, is
        // the difference between a reversal and a jump followed by a reversal.
        let (opacity_from, travel_from, chrome_from) = self
            .content_view_fade
            .as_ref()
            .map(|fade| {
                (
                    fade.opacity.value(now),
                    fade.travel.value(now),
                    fade.chrome_travel.value(now),
                )
            })
            .unwrap_or((from, from, from));
        // Time the reversal by how far it actually has to go. Replaying the
        // full duration to cover the last tenth of a journey reads as the
        // animation having stalled; the floor keeps a near-complete one from
        // snapping.
        let span = |start: f32| ((to - start).abs()).clamp(0.25, 1.0);
        self.content_view_fade = Some(crate::termwindow::content_view::ContentViewFade {
            opacity: crate::ui::anim::Timeline::new(
                now,
                opacity_from,
                to,
                CONTENT_VIEW_FADE.mul_f32(span(opacity_from)),
                crate::ui::anim::Easing::Smooth,
            ),
            travel: crate::ui::anim::Timeline::new(
                now,
                travel_from,
                to,
                CONTENT_VIEW_TRAVEL.mul_f32(span(travel_from)),
                crate::ui::anim::Easing::OutCubic,
            ),
            // Opening, the frame is seen leaving before the terminal starts
            // crossing; closing, it comes back only once the terminal has
            // landed. Same two events, opposite order, so the delay swaps ends.
            chrome_travel: crate::ui::anim::Timeline::delayed(
                now,
                chrome_from,
                to,
                if closing {
                    CHROME_RETURN_DELAY
                } else {
                    Duration::ZERO
                },
                CONTENT_VIEW_CHROME_TRAVEL.mul_f32(span(chrome_from)),
                crate::ui::anim::Easing::OutCubic,
            ),
            chrome: None,
            ghost,
            flight: None,
            landing: None,
            pending_destination: destination,
            started_at: now,
        });
        // Input is held for the duration, so a press that is still waiting for
        // its release must not be left half-finished: the release would be
        // swallowed and the drag would resume against the next stray motion.
        self.dragging = None;
    }

    pub(crate) fn close_content_view(&mut self) {
        if let Some(id) = self
            .content_view_response_tab_id
            .or(self.active_content_view_id)
        {
            self.close_content_view_by_id(id);
        }
    }

    pub(crate) fn close_content_view_by_id(&mut self, id: ContentViewId) {
        let was_foreground = self.active_content_view_id == Some(id);
        let Some(idx) = self.content_views.iter().position(|tab| tab.id == id) else {
            return;
        };
        let was_full_window = matches!(
            self.content_views[idx].view.presentation(),
            ContentViewPresentation::FullWindow
        );
        // Ask, while the view still exists, where the terminal it is giving
        // the window back to was sitting. After the removal there is nobody
        // left to answer.
        let departing_destination = Mux::get()
            .get_active_tab_for_window(self.mux_window_id)
            .map(|tab| tab.tab_id())
            .and_then(|tab_id| self.content_views[idx].view.terminal_landing_rect(tab_id));
        // Deliberately not gated on `was_foreground`: some close paths clear
        // the active id before removing the view, and a view that had been
        // painting is on screen whether or not it still holds that id. Having
        // a recorded frame is the evidence that matters.
        let was_on_screen = was_full_window && self.content_view_last_frame.is_some();
        self.content_views.remove(idx);
        if was_on_screen {
            self.begin_content_view_fade_to(true, departing_destination);
        }

        if was_foreground {
            self.active_content_view_id = self
                .content_views
                .iter()
                .take(idx)
                .rev()
                .chain(self.content_views.iter().skip(idx))
                .find(|tab| self.content_view_visible_in_active_space(tab))
                .map(|tab| tab.id);
        }
        self.sync_content_view_surfaces_with_mux();
        self.sync_workspace_sidebar_pending_thread_selection();

        let is_foreground = self.content_view_foreground();
        if crate::termwindow::content_view::take_deferred_mux_resize_on_exit(
            &mut self.content_view_deferred_mux_resize,
            was_foreground,
            is_foreground,
        ) {
            self.resize_mux_tabs_to_current_terminal_size();
        }
        self.invalidate_window();
    }

    pub(crate) fn request_close_content_view_by_id(&mut self, id: ContentViewId) {
        let response = self
            .content_view_mut_by_id(id)
            .map(|view| view.on_close_requested())
            .unwrap_or(crate::termwindow::content_view::ContentViewResponse::Ignored);
        self.handle_content_response_for(id, response);
    }

    pub(crate) fn show_onboarding(&mut self) {
        let space_name = crate::workspace_threads::active_space_name(&self.active_space_id)
            .unwrap_or_else(|| self.active_space_id.clone());
        self.open_content_view(Box::new(
            crate::termwindow::onboarding::OnboardingView::new(
                self.active_space_id.clone(),
                space_name,
            ),
        ));
    }

    pub(crate) fn maybe_show_onboarding(&mut self) {
        if crate::native_settings::should_show_onboarding(&crate::native_settings::load()) {
            self.show_onboarding();
        }
    }

    pub(crate) fn set_content_view_active(&mut self, active: bool) {
        if active {
            let id = self
                .active_content_view_id
                .filter(|id| {
                    self.content_views
                        .iter()
                        .any(|tab| tab.id == *id && self.content_view_visible_in_active_space(tab))
                })
                .or_else(|| {
                    self.content_views
                        .iter()
                        .rev()
                        .find(|tab| self.content_view_visible_in_active_space(tab))
                        .map(|tab| tab.id)
                });
            self.set_active_content_view_id(id);
        } else {
            self.set_active_content_view_id(None);
        }
    }

    pub(crate) fn activate_content_view(&mut self, id: ContentViewId) {
        if self
            .content_views
            .iter()
            .any(|tab| tab.id == id && self.content_view_visible_in_active_space(tab))
        {
            self.set_active_content_view_id(Some(id));
            self.reactivate_content_view_by_id(id);
        }
    }

    /// The card menu for the Remote Hosts page. Built here because a menu
    /// belongs to the window: the page has no window handle, and a native
    /// menu reports its choice back to the window anyway.
    pub(crate) fn show_remote_host_menu(
        &mut self,
        host_id: String,
        label: String,
        editable: bool,
        x: f32,
        y: f32,
    ) {
        use crate::termwindow::content_view::RemoteHostCommand;
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        self.begin_context_menu_application_actions();
        let mut items = vec![self.context_menu_application_item_with_icon(
            crate::i18n::tr("ssh-menu-connect"),
            ContextMenuIcon::Terminal,
            ContextMenuApplicationAction::RemoteHost {
                host_id: host_id.clone(),
                command: RemoteHostCommand::Connect,
            },
            true,
        )];
        if editable {
            items.push(ContextMenuItem::Separator);
            items.push(self.context_menu_application_item_with_icon(
                crate::i18n::tr("ssh-menu-edit"),
                ContextMenuIcon::Edit,
                ContextMenuApplicationAction::RemoteHost {
                    host_id: host_id.clone(),
                    command: RemoteHostCommand::Edit,
                },
                true,
            ));
            items.push(self.context_menu_application_item_with_icon(
                crate::i18n::tr("ssh-menu-duplicate"),
                ContextMenuIcon::Copy,
                ContextMenuApplicationAction::RemoteHost {
                    host_id: host_id.clone(),
                    command: RemoteHostCommand::Duplicate,
                },
                true,
            ));
            items.push(ContextMenuItem::Separator);
            // Deleting a host takes its threads and its mirrored Spaces with
            // it and cannot be undone, so it asks first.
            let confirm = self.context_menu_application_item_with_icon(
                crate::i18n::tr("ssh-menu-delete-confirm"),
                ContextMenuIcon::Delete,
                ContextMenuApplicationAction::RemoteHost {
                    host_id,
                    command: RemoteHostCommand::Delete,
                },
                true,
            );
            let mut args = fluent_bundle::FluentArgs::new();
            args.set("name", label);
            items.push(ContextMenuItem::submenu_with_icon(
                crate::i18n::tr_args("ssh-menu-delete-named", &args),
                ContextMenuIcon::Delete,
                vec![
                    ContextMenuItem::section_header(crate::i18n::tr("ssh-menu-delete-explain")),
                    confirm,
                ],
            ));
        }
        self.show_term_context_menu(&window, euclid::point2(x as isize, y as isize), items);
    }

    /// Open the SSH hosts page on a blank host form. Unlike
    /// `toggle_ssh_hosts_view` this never closes an open page: it is reached
    /// from a menu entry worded as an action, and an action that sometimes
    /// closes the page instead is not one.
    pub(crate) fn open_ssh_hosts_view_new_host(&mut self) {
        let key = crate::termwindow::ssh_hosts_view::SSH_HOSTS_CONTENT_VIEW_KEY;
        match self.content_view_id_for_key(key) {
            Some(id) => {
                self.activate_content_view(id);
                if let Some(view) = self.content_view_mut_by_id(id) {
                    view.begin_new_remote_host();
                }
            }
            None => {
                self.open_content_view(Box::new(
                    crate::termwindow::ssh_hosts_view::SshHostsView::new_host(),
                ));
            }
        }
    }

    /// Toggle the SSH hosts content view (sidebar button / OpenSshHosts).
    pub(crate) fn toggle_ssh_hosts_view(&mut self) {
        let key = crate::termwindow::ssh_hosts_view::SSH_HOSTS_CONTENT_VIEW_KEY;
        if self.active_content_view_key_is(key) {
            self.close_content_view();
        } else if let Some(id) = self.content_view_id_for_key(key) {
            self.activate_content_view(id);
        } else {
            self.open_content_view(Box::new(
                crate::termwindow::ssh_hosts_view::SshHostsView::new(),
            ));
        }
    }

    /// Toggle the global Safari-style live terminal overview. It is a
    /// full-window ContentView, but keeps the terminal grid and the user's
    /// sidebar preferences untouched behind it.
    pub(crate) fn toggle_live_overview_view(&mut self) {
        let key = crate::termwindow::live_overview::LIVE_OVERVIEW_CONTENT_VIEW_KEY;
        if self.active_content_view_key_is(key) {
            self.close_content_view();
        } else if let Some(id) = self.content_view_id_for_key(key) {
            self.activate_content_view(id);
        } else {
            let active_workspace = self
                .current_mux_workspace()
                .unwrap_or_else(|| Mux::get().active_workspace());
            // A card is a picture of the space the terminal occupies on
            // screen, and that is also what the opening transition flies from:
            // `terminal_content_rect`, grid plus padding. Sizing the card from
            // the bare grid instead left the two ends measuring different
            // rectangles of the same terminal -- the flight arrived stretched
            // by whatever share the padding held, non-uniformly, because the
            // card it landed in had never accounted for it.
            let content = self.terminal_content_rect();
            let host_preview_aspect = if content.size.height > 0.0 && content.size.width > 0.0 {
                content.size.width / content.size.height
            } else if self.terminal_size.pixel_height > 0 {
                self.terminal_size.pixel_width as f32 / self.terminal_size.pixel_height as f32
            } else {
                let width = self.terminal_size.cols as f32
                    * self.render_metrics.cell_size.width.max(1) as f32;
                let height = self.terminal_size.rows as f32
                    * self.render_metrics.cell_size.height.max(1) as f32;
                width / height.max(1.0)
            };
            self.open_content_view(Box::new(
                crate::termwindow::live_overview::LiveOverviewView::new(
                    self.space_owner_id,
                    &self.active_space_id,
                    &active_workspace,
                    host_preview_aspect,
                ),
            ));
        }
    }

    fn invalidate_window(&self) {
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// Copy the active content view's focused text to the clipboard (⌘C).
    pub(crate) fn content_view_copy(&mut self) {
        if let Some(text) = self.active_content_view().and_then(|v| v.copy_text()) {
            if !text.is_empty() {
                self.copy_to_clipboard(
                    config::keyassignment::ClipboardCopyDestination::Clipboard,
                    text,
                );
            }
        }
    }

    /// Cut the active content view's selected text to the clipboard (⌘X).
    pub(crate) fn content_view_cut(&mut self) {
        if let Some(text) = self.active_content_view_mut().and_then(|v| v.cut_text()) {
            if !text.is_empty() {
                self.copy_to_clipboard(
                    config::keyassignment::ClipboardCopyDestination::Clipboard,
                    text,
                );
                self.invalidate_window();
            }
        }
    }

    fn content_view_paste_from(&mut self, clipboard: ClipboardPasteSource) {
        let Some(view_id) = self.active_content_view_id else {
            return;
        };
        let Some(window) = self.window.as_ref().map(|w| w.clone()) else {
            return;
        };
        let clipboard = match clipboard {
            ClipboardPasteSource::Clipboard => ::window::Clipboard::Clipboard,
            ClipboardPasteSource::PrimarySelection => ::window::Clipboard::PrimarySelection,
        };
        let future = window.get_clipboard(clipboard);
        promise::spawn::spawn(async move {
            if let Ok(clip) = future.await {
                window.notify(TermWindowNotif::Apply(Box::new(move |myself| {
                    let resp = myself
                        .content_view_mut_by_id(view_id)
                        .map(|v| v.on_paste(&clip));
                    if let Some(resp) = resp {
                        myself.handle_content_response_for(view_id, resp);
                    }
                })));
            }
        })
        .detach();
    }

    /// Read the clipboard asynchronously and feed it to the active content view
    /// (used for ⌘V inside content-view text fields).
    pub(crate) fn content_view_paste(&mut self) {
        self.content_view_paste_from(ClipboardPasteSource::Clipboard);
    }

    /// Apply the result of an input event handled by the active content view.
    pub(crate) fn handle_content_response(
        &mut self,
        response: crate::termwindow::content_view::ContentViewResponse,
    ) {
        let Some(id) = self.active_content_view_id else {
            return;
        };
        self.handle_content_response_for(id, response);
    }

    pub(crate) fn handle_content_response_for(
        &mut self,
        id: ContentViewId,
        response: crate::termwindow::content_view::ContentViewResponse,
    ) {
        use crate::termwindow::content_view::ContentViewResponse;
        match response {
            ContentViewResponse::Ignored => return,
            ContentViewResponse::Redraw => {}
            ContentViewResponse::Close => {
                self.close_content_view_by_id(id);
                return;
            }
            ContentViewResponse::Run(func) => {
                let previous_response_tab_id = self.content_view_response_tab_id.replace(id);
                func(self);
                self.content_view_response_tab_id = previous_response_tab_id;
            }
        }
        self.invalidate_window();
    }

    fn invalidate_modal(&mut self) {
        if let Some(modal) = self.get_modal() {
            modal.reconfigure(self);
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }
    }

    pub fn cancel_modal(&self) {
        self.modal.borrow_mut().take();
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub fn set_modal(&self, modal: Rc<dyn Modal>) {
        self.modal.borrow_mut().replace(modal);
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    fn get_modal(&self) -> Option<Rc<dyn Modal>> {
        self.modal.borrow().as_ref().map(|m| Rc::clone(&m))
    }

    fn update_scrollbar(&mut self) {
        if !self.show_scroll_bar {
            return;
        }

        let tab = match self.get_active_pane_or_overlay() {
            Some(tab) => tab,
            None => return,
        };

        let render_dims = tab.get_dimensions();
        if render_dims == self.last_scroll_info {
            return;
        }

        self.last_scroll_info = render_dims;

        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    /// Called by various bits of code to update the title bar.
    /// Let's also trigger the status event so that it can choose
    /// to update the right-status.
    fn update_title(&mut self) {
        self.schedule_status_update();
        self.update_title_impl();
    }

    fn window_contains_pane(&mut self, pane_id: PaneId) -> bool {
        let mux = Mux::get();

        let (_domain, window_id, _tab_id) = match mux.resolve_pane_id(pane_id) {
            Some(tuple) => tuple,
            None => return false,
        };

        return window_id == self.mux_window_id;
    }

    fn emit_user_var_event(&mut self, pane_id: PaneId, name: String, value: String) {
        if !self.window_contains_pane(pane_id) {
            return;
        }

        let mux = Mux::get();
        let window = GuiWin::new(self);
        let pane = match mux.get_pane(pane_id) {
            Some(pane) => mux_lua::MuxPane(pane.pane_id()),
            None => return,
        };

        async fn do_event(
            lua: Option<Rc<mlua::Lua>>,
            name: String,
            value: String,
            window: GuiWin,
            pane: MuxPane,
        ) -> anyhow::Result<()> {
            if let Some(lua) = lua {
                let args = lua.pack_multi((window.clone(), pane, name, value))?;
                if let Err(err) =
                    config::lua::emit_event(&lua, ("user-var-changed".to_string(), args)).await
                {
                    log::error!("while processing user-var-changed event: {:#}", err);
                }
            }

            window
                .window
                .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    term_window.update_title();
                })));

            Ok(())
        }

        promise::spawn::spawn(config::with_lua_config_on_main_thread(move |lua| {
            do_event(lua, name, value, window, pane)
        }))
        .detach();
    }

    /// Called by window:set_right_status after the status has
    /// been updated; let's update the bar
    pub fn update_title_post_status(&mut self) {
        self.update_title_impl();
    }

    fn theme_aligned_tab_bar_colors(&mut self) -> TabBarColors {
        let palette = self.palette().clone();
        theme_aligned_tab_bar_colors_from_palette(&palette)
    }

    fn update_title_impl(&mut self) {
        let mux = Mux::get();
        let window = match mux.get_window(self.mux_window_id) {
            Some(window) => window,
            _ => return,
        };
        let tabs = self.get_tab_information();
        let panes = self.get_pane_information();
        let active_tab = tabs.iter().find(|t| t.is_active).cloned();
        let active_pane = panes.iter().find(|p| p.is_active).cloned();

        let border = self.get_os_border();
        let tab_bar_height = self.tab_bar_pixel_height().unwrap_or(0.);
        let tab_bar_y = if self.config.tab_bar_at_bottom {
            ((self.dimensions.pixel_height as f32) - (tab_bar_height + border.bottom.get() as f32))
                .max(0.)
        } else {
            border.top.get() as f32
        };

        let tab_bar_height = self.tab_bar_pixel_height().unwrap_or(0.);
        let tab_bar_x = self.tab_bar_left_edge();
        let tab_bar_width = self.dimensions.pixel_width.saturating_sub(tab_bar_x).max(1);

        let hovering_in_tab_bar = match &self.current_mouse_event {
            Some(event) => {
                let mouse_y = event.coords.y as f32;
                let mouse_x = event.coords.x.max(0) as usize;
                mouse_x >= tab_bar_x
                    && mouse_y >= tab_bar_y as f32
                    && mouse_y < tab_bar_y as f32 + tab_bar_height
            }
            None => false,
        };

        let has_explicit_tab_colors = self.config.resolved_palette.tab_bar.is_some();
        let themed_tab_bar_colors = if has_explicit_tab_colors {
            None
        } else {
            Some(self.theme_aligned_tab_bar_colors())
        };
        let tab_bar_colors = if has_explicit_tab_colors {
            self.config.resolved_palette.tab_bar.as_ref()
        } else {
            themed_tab_bar_colors.as_ref()
        };

        let new_tab_bar = TabBarState::new(
            (tab_bar_width / self.render_metrics.cell_size.width as usize).max(1),
            if hovering_in_tab_bar {
                self.current_mouse_event.as_ref().map(|event| {
                    event.coords.x.saturating_sub(tab_bar_x as isize).max(0) as usize
                        / self.render_metrics.cell_size.width as usize
                })
            } else {
                None
            },
            &tabs,
            &panes,
            tab_bar_colors,
            &self.config,
            crate::termwindow::ui::platform_chrome::uses_integrated_window_buttons(
                self.config.window_decorations,
                self.window_state,
            ),
            self.tab_bar_scroll_offset,
            &self.left_status,
            &self.right_status,
        );
        if new_tab_bar != self.tab_bar {
            self.tab_bar = new_tab_bar;
            self.invalidate_fancy_tab_bar();
            self.invalidate_modal();
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }

        let num_tabs = window.len();
        if num_tabs == 0 {
            return;
        }
        drop(window);

        let title = match config::run_immediate_with_lua_config(|lua| {
            if let Some(lua) = lua {
                let tabs = lua.create_sequence_from(tabs.clone().into_iter())?;
                let panes = lua.create_sequence_from(panes.clone().into_iter())?;

                let v = config::lua::emit_sync_callback(
                    &*lua,
                    (
                        "format-window-title".to_string(),
                        (
                            active_tab.clone(),
                            active_pane.clone(),
                            tabs,
                            panes,
                            (*self.config).clone(),
                        ),
                    ),
                )?;
                match &v {
                    mlua::Value::Nil => Ok(None),
                    _ => Ok(Some(String::from_lua(v, &*lua)?)),
                }
            } else {
                Ok(None)
            }
        }) {
            Ok(s) => s,
            Err(err) => {
                log::warn!("format-window-title: {}", err);
                None
            }
        };

        let mut title = match title {
            Some(title) => title,
            None => {
                if let (Some(pos), Some(tab)) = (active_pane, active_tab) {
                    if num_tabs == 1 {
                        format!("{}{}", if pos.is_zoomed { "[Z] " } else { "" }, pos.title)
                    } else {
                        format!(
                            "{}[{}/{}] {}",
                            if pos.is_zoomed { "[Z] " } else { "" },
                            tab.tab_index + 1,
                            num_tabs,
                            pos.title
                        )
                    }
                } else {
                    "".to_string()
                }
            }
        };

        if let Some(access) = self.active_frontend_access_state() {
            match access.mode {
                mux::FrontendAccessMode::TmuxLatest => {
                    title.push_str(" · A SHARED");
                    if let Some(state) = self
                        .active_remote_frontend_viewport_state()
                        .filter(|_| !self.owns_frontend_viewport())
                    {
                        title.push_str(&format!(
                            " · VIEW {}×{}",
                            state.canonical_size.cols, state.canonical_size.rows
                        ));
                    }
                }
                mux::FrontendAccessMode::Handoff => match self.frontend_terminal_gate() {
                    wezterm_client::domain::RemoteFrontendGate::Visible => {
                        title.push_str(" · B ACTIVE");
                    }
                    wezterm_client::domain::RemoteFrontendGate::Claimable { owner } => {
                        log::debug!(
                            "window {}: terminal gate is claimable (owner {:?})",
                            self.mux_window_id,
                            owner.as_ref().map(|owner| (owner.hostname.as_str(), owner.pid, owner.id))
                        );
                        let owner = owner
                            .as_ref()
                            .map(|owner| owner.hostname.as_str())
                            .filter(|hostname| !hostname.trim().is_empty())
                            .unwrap_or("available");
                        title.push_str(&format!(" · B VIEW on {owner} · click to continue"));
                    }
                    wezterm_client::domain::RemoteFrontendGate::Connecting => {
                        title.push_str(" · CONNECTING");
                    }
                    wezterm_client::domain::RemoteFrontendGate::Reconnecting => {
                        title.push_str(" · RECONNECTING");
                    }
                    wezterm_client::domain::RemoteFrontendGate::Offline => {
                        title.push_str(" · OFFLINE");
                    }
                    wezterm_client::domain::RemoteFrontendGate::Syncing => {
                        title.push_str(" · SYNCING");
                    }
                },
            }
        }

        if let Some(window) = self.window.as_ref() {
            window.set_title(&title);

            let show_tab_bar = if num_tabs == 1 {
                self.config.enable_tab_bar && !self.config.hide_tab_bar_if_only_one_tab
            } else {
                self.config.enable_tab_bar
            };

            // If the number of tabs changed and caused the tab bar to
            // hide/show, then we'll need to resize things.  It is simplest
            // to piggy back on the config reloading code for that, so that
            // is what we're doing.
            if show_tab_bar != self.show_tab_bar {
                self.config_was_reloaded();
            }
        }
        self.schedule_next_status_update();
    }

    fn active_terminal_has_overlay(&self, tab: &Arc<Tab>) -> bool {
        if self
            .tab_state
            .borrow()
            .get(&tab.tab_id())
            .is_some_and(|state| state.overlay.is_some())
        {
            return true;
        }
        let pane_state = self.pane_state.borrow();
        tab.iter_panes().into_iter().any(|pos| {
            pane_state
                .get(&pos.pane.pane_id())
                .is_some_and(|state| state.overlay.is_some())
        })
    }

    fn can_track_presented_terminal_output(&self, tab: &Arc<Tab>) -> bool {
        !self.content_view_foreground()
            && self.content_view_fade.is_none()
            && !self.frontend_surface_blocked()
            && !self.active_terminal_has_overlay(tab)
    }

    fn acknowledge_presented_pane_output(&mut self) {
        let presented = std::mem::take(&mut self.frame_pane_output_generations);
        let mut pane_state = self.pane_state.borrow_mut();
        for (pane_id, generation) in presented {
            pane_state
                .entry(pane_id)
                .or_default()
                .presented_output_generation = generation;
        }
    }

    fn discard_unpresented_pane_output(&mut self) {
        self.frame_pane_output_generations.clear();
    }

    /// PaneOutput is durable in the Mux, while delivery into a native window
    /// is asynchronous. Compare that durable generation with this window's
    /// last successful present so a lost last repaint is observable for every
    /// pane type. ClientPane gets an additional state watchdog for work that
    /// has not yet become a renderable line.
    fn terminal_render_watchdog(&mut self) {
        let mux = Mux::get();
        let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) else {
            return;
        };
        // These surfaces replace or deliberately hide terminal pixels. They
        // cannot acknowledge a terminal generation, so retrying underneath
        // them would latch the watchdog at one repaint per status tick.
        if !self.can_track_presented_terminal_output(&tab) {
            return;
        }
        for pos in tab.iter_panes() {
            if self.collapsed_pane_layouts.contains_key(&pos.pane_stack_id) {
                continue;
            }
            let pane_id = pos.pane.pane_id();
            let current = mux.pane_output_generation(pane_id);
            let presented = self
                .pane_state
                .borrow()
                .get(&pane_id)
                .map(|state| state.presented_output_generation)
                .unwrap_or(0);
            if !pane_output_needs_repaint(current, presented) {
                self.watchdog_presented_seen.remove(&pane_id);
            } else {
                // A pane whose output arrives faster than it is presented
                // is behind at every tick while painting perfectly well:
                // a remote pane streaming pictures, say. Behind is a lost
                // frame only when what was presented has not moved since
                // the last tick that found the pane behind.
                let seen = self.watchdog_presented_seen.insert(pane_id, presented);
                if seen != Some(presented) {
                    continue;
                }
                log::debug!(
                    "terminal render watchdog: window={} pane={pane_id} output generation \
                     {current} has not been presented (last={presented}); repainting",
                    self.mux_window_id,
                );
                // One tick behind is a lost frame; a RUN of ticks means
                // PaneOutput is not reaching this window at all — the fast
                // path would have painted long before the next 1s tick.
                // Rebuild the subscription so the failure heals in seconds
                // instead of lasting for the window's life. Two guards keep
                // this from misfiring: increments are paced (status updates
                // can run this in sub-second bursts, which must not count
                // as multiple seconds behind), and the heal fires exactly
                // once per episode — a pane that legitimately never
                // presents (a hidden stack member) latches the watchdog
                // forever and must not drive a resubscribe loop. Occluded
                // windows are exempt: AppKit suppresses their draws, so
                // falling behind there is expected.
                let paced = self
                    .watchdog_last_forced
                    .is_none_or(|at| at.elapsed() >= Duration::from_millis(700));
                if paced {
                    self.watchdog_last_forced = Some(Instant::now());
                    self.watchdog_forced_repaints = self.watchdog_forced_repaints.saturating_add(1);
                    if self.watchdog_forced_repaints == 3 && self.occluded.is_none() {
                        log::warn!(
                            "terminal render watchdog: window={} forced 3 consecutive repaints; \
                             rebuilding the mux pane-update subscription",
                            self.mux_window_id,
                        );
                        self.subscribe_to_pane_updates();
                    }
                    // The forced repaint rides the same pacing: status
                    // updates can run this watchdog in sub-second bursts,
                    // and a latched pane (one that never presents) would
                    // otherwise turn each burst into a repaint storm —
                    // sustained forced painting pins GPU/staging memory
                    // that the usage-window shrink cannot reclaim.
                    if let Some(window) = self.window.as_ref() {
                        window.invalidate();
                    }
                }
                return;
            }
            if let Some(client_pane) = pos.pane.downcast_ref::<wezterm_client::pane::ClientPane>() {
                // Pass the viewport the renderer will actually paint —
                // scrolled back, that is not the live screen, and judging
                // the live rows would latch the watchdog on Stale rows a
                // repaint never touches.
                let viewport_top = self.get_viewport(pane_id);
                if client_pane.render_looks_stalled(viewport_top) {
                    log::debug!(
                        "terminal render watchdog: pane {pane_id} has undelivered content; repainting"
                    );
                    if let Some(window) = self.window.as_ref() {
                        window.invalidate();
                    }
                    return;
                }
            }
        }
        // A clean pass: every pane's output has been presented, so the
        // notification path is delivering again. Re-arms the once-per-
        // episode heal above.
        self.watchdog_forced_repaints = 0;
        self.watchdog_last_forced = None;
    }

    fn schedule_next_status_update(&mut self) {
        if let Some(window) = self.window.as_ref() {
            let now = Instant::now();
            if self.last_status_call <= now {
                let interval = Duration::from_millis(self.config.status_update_interval);
                let target = now + interval;
                self.last_status_call = target;

                let window = window.clone();
                promise::spawn::spawn(async move {
                    Timer::at(target).await;
                    window.notify(TermWindowNotif::EmitStatusUpdate);
                })
                .detach();
            }
        }
    }

    /// Where the IME puts its candidate window: the cursor cell in the exact
    /// coordinates `paint_pane` drew it with. Deriving it from pane dimensions
    /// alone drifted on mux panes (viewport ahead of physical_top, pane nav
    /// bar, per-pane font scale), landing the popup rows above the text.
    pub(crate) fn update_text_cursor(
        &mut self,
        cursor: &mux::renderable::StableCursorPosition,
        stable_top: StableRowIndex,
        dims: &RenderableDimensions,
        left_pixel_x: f32,
        top_pixel_y: f32,
        cell_size: Size,
    ) {
        let Some(win) = self.window.as_ref() else {
            return;
        };
        let last_row = dims.viewport_rows.saturating_sub(1) as isize;
        let row = (cursor.y - stable_top).clamp(0, last_row);
        let col = cursor.x.min(dims.cols.saturating_sub(1)) as isize;
        let r = Rect::new(
            Point::new(
                left_pixel_x as isize + col * cell_size.width,
                top_pixel_y as isize + row * cell_size.height,
            ),
            cell_size,
        );
        win.set_text_cursor_position(r);
    }

    fn activate_window(&mut self, window_idx: usize) -> anyhow::Result<()> {
        let windows = front_end().gui_windows();
        if let Some(win) = windows.get(window_idx) {
            win.window.focus();
        }
        Ok(())
    }

    fn activate_window_relative(&mut self, delta: isize, wrap: bool) -> anyhow::Result<()> {
        let windows = front_end().gui_windows();
        let my_idx = windows
            .iter()
            .position(|w| Some(&w.window) == self.window.as_ref())
            .ok_or_else(|| anyhow!("I'm not in the window list!?"))?;

        let idx = my_idx as isize + delta;

        let idx = if wrap {
            let idx = if idx < 0 {
                windows.len() as isize + idx
            } else {
                idx
            };
            idx as usize % windows.len()
        } else {
            if idx < 0 {
                0
            } else if idx >= windows.len() as isize {
                windows.len().saturating_sub(1)
            } else {
                idx as usize
            }
        };

        if let Some(win) = windows.get(idx) {
            win.window.focus();
        }

        Ok(())
    }

    fn activate_tab(&mut self, tab_idx: isize) -> anyhow::Result<()> {
        let mux = Mux::get();
        let mut window = mux
            .get_window_mut(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        // This logic is coupled with the CliSubCommand::ActivateTab
        // logic in wezterm/src/main.rs. If you update this, update that!
        let max = window.len();

        let tab_idx = if tab_idx < 0 {
            max.saturating_sub(tab_idx.abs() as usize)
        } else {
            tab_idx as usize
        };

        if tab_idx < max {
            window.save_and_then_set_active(tab_idx);

            drop(window);

            if let Some(tab) = self.get_active_pane_or_overlay() {
                tab.focus_changed(true);
            }

            self.update_title();
            self.update_scrollbar();
            // A discrete top-level tab switch must converge before its first
            // paint; it is not part of the 120ms continuous-window-resize
            // debounce. This never claims a passive tab.
            self.sync_active_tab_geometry_now();
            self.persist_workspace_layout_after_mutation("active tab changed");
        }
        Ok(())
    }

    fn activate_tab_relative(&mut self, delta: isize, wrap: bool) -> anyhow::Result<()> {
        let mux = Mux::get();
        let window = mux
            .get_window(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        let max = window.len();
        ensure!(max > 0, "no more tabs");

        // This logic is coupled with the CliSubCommand::ActivateTab
        // logic in wezterm/src/main.rs. If you update this, update that!
        let active = window.get_active_idx() as isize;
        let tab = active + delta;
        let tab = if wrap {
            let tab = if tab < 0 { max as isize + tab } else { tab };
            (tab as usize % max) as isize
        } else {
            if tab < 0 {
                0
            } else if tab >= max as isize {
                max as isize - 1
            } else {
                tab
            }
        };
        drop(window);
        self.activate_tab(tab)
    }

    fn activate_last_tab(&mut self) -> anyhow::Result<()> {
        let mux = Mux::get();
        let window = mux
            .get_window(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        let last_idx = window.get_last_active_idx();
        drop(window);
        match last_idx {
            Some(idx) => self.activate_tab(idx as isize),
            None => Ok(()),
        }
    }

    fn move_tab(&mut self, tab_idx: usize) -> anyhow::Result<()> {
        let mux = Mux::get();
        let mut window = mux
            .get_window_mut(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        let max = window.len();
        ensure!(max > 0, "no more tabs");

        let active = window.get_active_idx();

        ensure!(tab_idx < max, "cannot move a tab out of range");

        let tab_inst = window.remove_by_idx(active);
        window.insert(tab_idx, &tab_inst);
        window.set_active_without_saving(tab_idx);

        drop(window);
        self.update_title();
        self.update_scrollbar();
        self.persist_workspace_layout_after_mutation("tab moved");

        Ok(())
    }

    fn show_input_selector(&mut self, args: &config::keyassignment::InputSelector) {
        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        // Ignore any current overlay: we're going to cancel it out below
        // and we don't want this new one to reference that cancelled pane
        let pane = match self.get_active_pane_no_overlay() {
            Some(pane) => pane,
            None => return,
        };

        let args = args.clone();

        let gui_win = GuiWin::new(self);
        let pane = MuxPane(pane.pane_id());

        let (overlay, future) = start_overlay(self, &tab, move |_tab_id, term| {
            crate::overlay::selector::selector(term, args, gui_win, pane)
        });
        self.assign_overlay(tab.tab_id(), overlay);
        promise::spawn::spawn(future).detach();
    }

    fn show_prompt_input_line(&mut self, args: &PromptInputLine) {
        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        let pane = match self.get_active_pane_or_overlay() {
            Some(pane) => pane,
            None => return,
        };

        let args = args.clone();

        let gui_win = GuiWin::new(self);
        let pane = MuxPane(pane.pane_id());

        let (overlay, future) = start_overlay(self, &tab, move |_tab_id, term| {
            crate::overlay::prompt::show_line_prompt_overlay(term, args, gui_win, pane)
        });
        self.assign_overlay(tab.tab_id(), overlay);
        promise::spawn::spawn(future).detach();
    }

    fn prompt_rename_current_tab(&mut self) {
        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        let tab_id = tab.tab_id();
        let mut initial_title = tab.get_title();
        if initial_title.is_empty() {
            initial_title = tab
                .get_active_pane()
                .map(|pane| pane.get_title())
                .unwrap_or_default();
        }

        self.inline_tab_rename = Some(InlineTabRename::new(
            InlineTabRenameTarget::WindowTab(tab_id),
            initial_title,
        ));
        self.update_title_impl();
    }

    fn prompt_rename_pane_tab(&mut self, pane_id: PaneId) {
        let initial_title = self
            .pane_tab_title_overrides
            .get(&pane_id)
            .cloned()
            .or_else(|| {
                Mux::get()
                    .get_pane(pane_id)
                    .map(|pane| ui::terminal_title_for_display(&pane.get_title()).to_string())
            })
            .unwrap_or_default();

        self.inline_tab_rename = Some(InlineTabRename::new(
            InlineTabRenameTarget::PaneTab(pane_id),
            initial_title,
        ));
        self.update_title_impl();
    }

    fn prompt_rename_project(&mut self, project_id: String) {
        let initial_title = crate::workspace_threads::project_name(&project_id).unwrap_or_default();
        self.inline_tab_rename = Some(InlineTabRename::new(
            InlineTabRenameTarget::Project(project_id),
            initial_title,
        ));
        self.update_title_impl();
    }

    fn prompt_rename_workspace_thread(&mut self, thread_id: String) {
        let initial_title = crate::workspace_threads::thread_name(&thread_id).unwrap_or_default();
        self.inline_tab_rename = Some(InlineTabRename::new(
            InlineTabRenameTarget::WorkspaceThread(thread_id),
            initial_title,
        ));
        self.update_title_impl();
    }

    fn prompt_rename_space(&mut self, space_id: String) {
        let initial_title = crate::workspace_threads::active_space_name(&space_id)
            .unwrap_or_else(|| "Space".to_string());
        self.inline_tab_rename = Some(InlineTabRename::new(
            InlineTabRenameTarget::Space(space_id),
            initial_title,
        ));
        self.update_title_impl();
    }

    pub(crate) fn prompt_create_project(&mut self, context: &dyn WindowOps) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };

        // In a mux-domain Space the project path lives on the remote server;
        // a local folder picker is meaningless there. Offer the remote
        // directories we actually know about (live pane cwds via OSC7 and
        // previously-added project paths) plus free-form entry.
        if let Some(domain_name) =
            crate::workspace_threads::client_domain_for_space(&self.active_space_id)
        {
            let mux = Mux::get();
            let Some(tab) = mux.get_active_tab_for_window(self.mux_window_id) else {
                return;
            };

            let mut candidates: Vec<String> = Vec::new();
            if let Some(domain) = mux.get_domain_by_name(&domain_name) {
                let domain_id = domain.domain_id();
                for pane in mux.iter_panes() {
                    if pane.domain_id() != domain_id {
                        continue;
                    }
                    if let Some(url) =
                        pane.get_current_working_dir(mux::pane::CachePolicy::AllowStale)
                    {
                        let path = url.path().to_string();
                        if !path.is_empty() && !candidates.contains(&path) {
                            candidates.push(path);
                        }
                    }
                }
            }
            for path in crate::workspace_threads::project_paths_for_space(&self.active_space_id) {
                if !candidates.contains(&path) {
                    candidates.push(path);
                }
            }

            if !candidates.iter().any(|c| c == "~") {
                candidates.push("~".to_string());
            }

            let description = format!(
                "Add a project on {domain_name}: pick a remote directory below,\n\
                 type to filter, or type a path (~/dir or /dir)."
            );

            let (overlay, future) = start_overlay(self, &tab, move |_tab_id, term| {
                let picked = crate::overlay::prompt::pick_path_prompt_overlay(
                    term,
                    &description,
                    "path> ",
                    candidates,
                )?;
                if let Some(path) = picked.filter(|path| !path.is_empty()) {
                    window.notify(TermWindowNotif::OpenProjectPath(PathBuf::from(path)));
                }
                Ok(())
            });
            self.assign_overlay(tab.tab_id(), overlay);
            promise::spawn::spawn(future).detach();
            return;
        }

        context.pick_folder_async(Box::new(move |path| {
            if let Some(path) = path {
                window.notify(TermWindowNotif::OpenProjectPath(path));
            }
        }));
    }

    fn reveal_project_in_folder(&self, project_id: &str) {
        if let Some(path) = crate::workspace_threads::project_reveal_path(project_id) {
            wezterm_open_url::reveal_path(&path);
        }
    }

    /// Whether the given pressed UI item is the surface currently hosting the
    /// Whether a hover tag may exist for this item right now. The static
    /// half lives in [`tooltip_label_for`]; this adds the conditions only
    /// the live window knows: a row being renamed shows an editor where its
    /// name was (the tag would name the file the user is renaming away
    /// from), and a file row whose painted label was not actually cut this
    /// frame already reads in full. Checked both when the tag arms
    /// (`update_hover_tooltip`) and when it paints (`paint_hover_tooltip`),
    /// so a rename started from a key assignment with the pointer at rest
    /// still suppresses an already-armed tag.
    pub(crate) fn hover_tooltip_allowed(&self, item_type: &UIItemType) -> bool {
        if self.ui_item_hosts_inline_rename(Some(item_type)) {
            return false;
        }
        match item_type {
            UIItemType::RightSidebarFileRow(path) => {
                self.right_sidebar_truncated_file_rows.contains(path)
            }
            UIItemType::RightSidebarRemoteFileRow(path) => {
                self.right_sidebar_truncated_remote_file_rows.contains(path)
            }
            _ => true,
        }
    }

    /// inline rename editor; presses there must not auto-commit the rename.
    fn ui_item_hosts_inline_rename(&self, item: Option<&UIItemType>) -> bool {
        let Some(rename) = self.inline_tab_rename.as_ref() else {
            return false;
        };
        let Some(item) = item else {
            return false;
        };
        match (&rename.target, item) {
            (
                InlineTabRenameTarget::WindowTab(tab_id),
                UIItemType::TabBar(TabBarItem::Tab { tab_idx, .. }),
            ) => {
                Mux::get()
                    .get_window(self.mux_window_id)
                    .and_then(|window| window.get_by_idx(*tab_idx).map(|tab| tab.tab_id()))
                    == Some(*tab_id)
            }
            (InlineTabRenameTarget::PaneTab(pane_id), UIItemType::PaneNav { pane_id: p, .. }) => {
                p == pane_id
            }
            (InlineTabRenameTarget::Space(_), UIItemType::SpaceMenu) => true,
            (InlineTabRenameTarget::Project(id), UIItemType::Project(other)) => id == other,
            (InlineTabRenameTarget::WorkspaceThread(id), UIItemType::WorkspaceThread(other)) => {
                id == other
            }
            (InlineTabRenameTarget::File(path), UIItemType::RightSidebarFileRow(other)) => {
                path == other
            }
            (
                InlineTabRenameTarget::RemoteFile { path, .. },
                UIItemType::RightSidebarRemoteFileRow(other),
            ) => path == other,
            _ => false,
        }
    }

    fn finish_inline_tab_rename(&mut self, commit: bool) {
        let rename = match self.inline_tab_rename.take() {
            Some(rename) => rename,
            None => return,
        };

        if commit {
            let title = rename.input.text().trim().to_string();
            match rename.target {
                InlineTabRenameTarget::WindowTab(tab_id) => {
                    if let Some(tab) = Mux::get().get_tab(tab_id) {
                        tab.set_title(&title);
                    }
                }
                InlineTabRenameTarget::PaneTab(pane_id) => {
                    if title.is_empty() {
                        self.pane_tab_title_overrides.remove(&pane_id);
                    } else {
                        self.pane_tab_title_overrides.insert(pane_id, title);
                    }
                }
                InlineTabRenameTarget::Space(space_id) => {
                    crate::workspace_threads::rename_space(&space_id, title);
                }
                InlineTabRenameTarget::Project(project_id) => {
                    crate::workspace_threads::rename_project(&project_id, title);
                }
                InlineTabRenameTarget::WorkspaceThread(thread_id) => {
                    crate::workspace_threads::rename_thread(&thread_id, title);
                }
                InlineTabRenameTarget::File(path) => {
                    self.commit_sidebar_file_rename(path, &title);
                }
                InlineTabRenameTarget::RemoteFile { path, source_key } => {
                    self.commit_sidebar_remote_file_rename(path, source_key, &title);
                }
            }
        }

        self.update_title_impl();
    }

    pub(crate) fn start_sidebar_file_rename(&mut self, path: PathBuf) {
        self.finish_inline_tab_rename(true);
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            return;
        };
        self.inline_tab_rename = Some(InlineTabRename::new(
            InlineTabRenameTarget::File(path),
            name,
        ));
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    fn commit_sidebar_file_rename(&mut self, old_path: PathBuf, new_name: &str) {
        if new_name.is_empty() || new_name.contains(std::path::is_separator) {
            return;
        }
        if old_path.file_name().map(|n| n.to_string_lossy()) == Some(new_name.into()) {
            return;
        }
        let Some(parent) = old_path.parent() else {
            return;
        };
        let new_path = parent.join(new_name);
        if remote_files::local_path_is_occupied(&new_path) {
            log::warn!(
                "not renaming {} to {new_name}: target already exists",
                old_path.display()
            );
            return;
        }
        if let Err(err) = std::fs::rename(&old_path, &new_path) {
            log::error!("failed to rename {}: {err:#}", old_path.display());
            return;
        }
        if self.right_sidebar_file_selected.as_ref() == Some(&old_path) {
            self.right_sidebar_file_selected = Some(new_path);
        }
        self.force_right_sidebar_file_rescan();
    }

    pub(crate) fn start_sidebar_remote_file_rename(&mut self, path: remote_files::RemotePath) {
        self.finish_inline_tab_rename(true);
        let Some(source_key) = self.right_sidebar_remote_files.current_source_key() else {
            return;
        };
        self.inline_tab_rename = Some(InlineTabRename::new(
            InlineTabRenameTarget::RemoteFile {
                path: path.clone(),
                source_key,
            },
            path.file_name().to_string(),
        ));
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    fn commit_sidebar_remote_file_rename(
        &mut self,
        old: remote_files::RemotePath,
        source_key: String,
        new_name: &str,
    ) {
        if new_name.is_empty() || old.file_name() == new_name {
            return;
        }
        if self
            .right_sidebar_remote_files
            .current_source_key()
            .as_deref()
            != Some(source_key.as_str())
        {
            // The editor belonged to a different tree. Never reinterpret its
            // absolute path against whichever host is visible now.
            return;
        }
        let Some(parent) = old.parent() else {
            return;
        };
        // join_name refuses separators, `..` and NUL; anything else is the
        // server's to accept or reject.
        let new = match parent.join_name(new_name) {
            Ok(new) => new,
            Err(err) => {
                self.right_sidebar_remote_files.error_message = Some(err);
                self.invalidate_window();
                return;
            }
        };
        let Some(origin) = self.current_remote_operation_origin() else {
            self.right_sidebar_remote_files.error_message =
                Some("Remote Files source is no longer available".to_string());
            self.invalidate_window();
            return;
        };
        let Some((backend, operation_lease, connection_key, connection_id)) =
            self.remote_transfer_handles(&origin)
        else {
            self.right_sidebar_remote_files.error_message =
                Some("Remote Files connection is no longer available".to_string());
            self.invalidate_window();
            return;
        };
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        promise::spawn::spawn(async move {
            // No-overwrite by contract: a taken name is the server's failure
            // to report, and shows up on the panel's notice line.
            let result = backend.rename(old.clone(), new).await;
            drop(operation_lease);
            let connection_died = result.as_ref().is_err_and(|message| {
                remote_files::invalidate_remote_connection_if_dead(
                    &connection_key,
                    connection_id,
                    message,
                )
            });
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                // A rename and a delete leave the tree in the same state as
                // far as the OLD path is concerned: gone. Same tail.
                term_window.finish_remote_entry_removal(
                    old,
                    result,
                    origin,
                    connection_key,
                    connection_id,
                    connection_died,
                );
            })));
        })
        .detach();
    }

    pub(crate) fn is_renaming_sidebar_remote_file(&self, path: &remote_files::RemotePath) -> bool {
        self.inline_tab_rename.as_ref().is_some_and(
            |rename| matches!(&rename.target, InlineTabRenameTarget::RemoteFile { path: p, .. } if p == path),
        )
    }

    pub(crate) fn sidebar_remote_file_rename_input(
        &self,
        path: &remote_files::RemotePath,
    ) -> Option<&TextInputState> {
        self.inline_tab_rename.as_ref().and_then(|rename| {
            matches!(
                &rename.target,
                InlineTabRenameTarget::RemoteFile { path: current, .. } if current == path
            )
            .then_some(&rename.input)
        })
    }

    pub(crate) fn is_renaming_sidebar_file(&self, path: &Path) -> bool {
        self.inline_tab_rename.as_ref().is_some_and(
            |rename| matches!(&rename.target, InlineTabRenameTarget::File(p) if p == path),
        )
    }

    pub(crate) fn sidebar_file_rename_input(&self, path: &Path) -> Option<&TextInputState> {
        self.inline_tab_rename.as_ref().and_then(|rename| {
            matches!(&rename.target, InlineTabRenameTarget::File(current) if current == path)
                .then_some(&rename.input)
        })
    }

    fn inline_window_tab_rename_title(&self, tab_id: TabId) -> Option<String> {
        self.inline_tab_rename
            .as_ref()
            .filter(|rename| matches!(rename.target, InlineTabRenameTarget::WindowTab(id) if id == tab_id))
            .map(|rename| rename.display_text())
    }

    fn inline_window_tab_rename_tab_id(&self) -> Option<TabId> {
        self.inline_tab_rename
            .as_ref()
            .and_then(|rename| match rename.target {
                InlineTabRenameTarget::WindowTab(tab_id) => Some(tab_id),
                InlineTabRenameTarget::PaneTab(_)
                | InlineTabRenameTarget::Space(_)
                | InlineTabRenameTarget::Project(_)
                | InlineTabRenameTarget::WorkspaceThread(_)
                | InlineTabRenameTarget::File(_)
                | InlineTabRenameTarget::RemoteFile { .. } => None,
            })
    }

    pub fn pane_nav_tab_title(&self, pane_id: PaneId, title: &str) -> String {
        if let Some(title) = self
            .inline_tab_rename
            .as_ref()
            .filter(|rename| matches!(rename.target, InlineTabRenameTarget::PaneTab(id) if id == pane_id))
            .map(|rename| rename.display_text())
        {
            return title;
        }

        self.pane_tab_title_overrides
            .get(&pane_id)
            .cloned()
            .unwrap_or_else(|| ui::terminal_title_for_display(title).to_string())
    }

    pub fn is_renaming_pane_nav_tab(&self, pane_id: PaneId) -> bool {
        self.inline_tab_rename.as_ref().is_some_and(
            |rename| matches!(rename.target, InlineTabRenameTarget::PaneTab(id) if id == pane_id),
        )
    }

    pub fn sidebar_project_title(&self, project_id: &str, name: &str) -> String {
        self.inline_tab_rename
            .as_ref()
            .filter(|rename| matches!(&rename.target, InlineTabRenameTarget::Project(id) if id == project_id))
            .map(|rename| rename.display_text())
            .unwrap_or_else(|| name.to_string())
    }

    pub fn sidebar_space_title(&self, space_id: &str, name: &str) -> String {
        self.inline_tab_rename
            .as_ref()
            .filter(|rename| matches!(&rename.target, InlineTabRenameTarget::Space(id) if id == space_id))
            .map(|rename| rename.display_text())
            .unwrap_or_else(|| name.to_string())
    }

    pub fn sidebar_thread_title(&self, thread_id: &str, name: &str) -> String {
        self.inline_tab_rename
            .as_ref()
            .filter(|rename| matches!(&rename.target, InlineTabRenameTarget::WorkspaceThread(id) if id == thread_id))
            .map(|rename| rename.display_text())
            .unwrap_or_else(|| name.to_string())
    }

    pub fn is_renaming_sidebar_thread(&self, thread_id: &str) -> bool {
        self.inline_tab_rename.as_ref().is_some_and(
            |rename| matches!(&rename.target, InlineTabRenameTarget::WorkspaceThread(id) if id == thread_id),
        )
    }

    fn show_confirmation(&mut self, args: &Confirmation) {
        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        let pane = match self.get_active_pane_or_overlay() {
            Some(pane) => pane,
            None => return,
        };

        let args = args.clone();

        let gui_win = GuiWin::new(self);
        let pane = MuxPane(pane.pane_id());

        let (overlay, future) = start_overlay(self, &tab, move |_tab_id, term| {
            crate::overlay::confirm::show_confirmation_overlay(term, args, gui_win, pane)
        });
        self.assign_overlay(tab.tab_id(), overlay);
        promise::spawn::spawn(future).detach();
    }

    fn show_debug_overlay(&mut self) {
        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        let gui_win = GuiWin::new(self);

        let opengl_info = self.opengl_info.as_deref().unwrap_or("Unknown").to_string();
        let connection_info = self.connection_name.clone();

        let (overlay, future) = start_overlay(self, &tab, move |_tab_id, term| {
            crate::overlay::show_debug_overlay(term, gui_win, opengl_info, connection_info)
        });
        self.assign_overlay(tab.tab_id(), overlay);
        promise::spawn::spawn(future).detach();
    }

    fn show_tab_navigator(&mut self) {
        let mux = Mux::get();
        let active_tab_idx = match mux.get_window(self.mux_window_id) {
            Some(mux_window) => mux_window.get_active_idx(),
            None => return,
        };
        let title = "Tab Navigator".to_string();
        let args = LauncherActionArgs {
            title: Some(title),
            flags: LauncherFlags::TABS,
            help_text: None,
            fuzzy_help_text: None,
            alphabet: None,
        };
        self.show_launcher_impl(args, active_tab_idx);
    }

    fn show_launcher(&mut self) {
        let title = "Launcher".to_string();
        let args = LauncherActionArgs {
            title: Some(title),
            flags: LauncherFlags::LAUNCH_MENU_ITEMS
                | LauncherFlags::WORKSPACES
                | LauncherFlags::DOMAINS
                | LauncherFlags::KEY_ASSIGNMENTS
                | LauncherFlags::COMMANDS,
            help_text: None,
            fuzzy_help_text: None,
            alphabet: None,
        };
        self.show_launcher_impl(args, 0);
    }

    fn show_launcher_impl(&mut self, args: LauncherActionArgs, initial_choice_idx: usize) {
        let mux_window_id = self.mux_window_id;
        let window = self.window.as_ref().unwrap().clone();

        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };

        let pane = match self.get_active_pane_or_overlay() {
            Some(pane) => pane,
            None => return,
        };

        let domain_id_of_current_pane = tab
            .get_active_pane()
            .expect("tab has no panes!")
            .domain_id();
        let pane_id = pane.pane_id();
        let tab_id = tab.tab_id();
        let title = args.title.unwrap();
        let flags = args.flags;
        let help_text = args.help_text.unwrap_or(
            "Select an item and press Enter=launch  \
             Esc=cancel  /=filter"
                .to_string(),
        );
        let fuzzy_help_text = args
            .fuzzy_help_text
            .unwrap_or("Fuzzy matching: ".to_string());

        let config = &self.config;
        let alphabet = args.alphabet.unwrap_or(config.launcher_alphabet.clone());

        promise::spawn::spawn(async move {
            let args = LauncherArgs::new(
                &title,
                flags,
                mux_window_id,
                pane_id,
                domain_id_of_current_pane,
                &help_text,
                &fuzzy_help_text,
                &alphabet,
            )
            .await;

            let win = window.clone();
            win.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let mux = Mux::get();
                if let Some(tab) = mux.get_tab(tab_id) {
                    let window = window.clone();
                    let (overlay, future) =
                        start_overlay(term_window, &tab, move |_tab_id, term| {
                            launcher(args, term, window, initial_choice_idx)
                        });

                    term_window.assign_overlay(tab_id, overlay);
                    promise::spawn::spawn(future).detach();
                }
            })));
        })
        .detach();
    }

    /// Returns the Prompt semantic zones
    fn get_semantic_prompt_zones(&mut self, pane: &Arc<dyn Pane>) -> &[StableRowIndex] {
        let cache = self
            .semantic_zones
            .entry(pane.pane_id())
            .or_insert_with(SemanticZoneCache::default);

        let seqno = pane.get_current_seqno();
        if cache.seqno != seqno {
            let zones = pane.get_semantic_zones().unwrap_or_else(|_| vec![]);
            let mut zones: Vec<StableRowIndex> = zones
                .into_iter()
                .filter_map(|zone| {
                    if zone.semantic_type == wezterm_term::SemanticType::Prompt {
                        Some(zone.start_y)
                    } else {
                        None
                    }
                })
                .collect();
            // dedup to avoid issues where both left and right prompts are
            // defined: we only care if there were 1+ prompts on a line,
            // not about how many prompts are on a line.
            // <https://github.com/wezterm/wezterm/issues/1121>
            zones.dedup();
            cache.zones = zones;
            cache.seqno = seqno;
        }
        &cache.zones
    }

    fn scroll_to_prompt(&mut self, amount: isize, pane: &Arc<dyn Pane>) -> anyhow::Result<()> {
        let dims = pane.get_dimensions();
        let position = self
            .get_viewport(pane.pane_id())
            .unwrap_or(dims.physical_top);
        let zone = {
            let zones = self.get_semantic_prompt_zones(&pane);
            let idx = match zones.binary_search(&position) {
                Ok(idx) | Err(idx) => idx,
            };
            let idx = ((idx as isize) + amount).max(0) as usize;
            zones.get(idx).cloned()
        };
        if let Some(zone) = zone {
            self.set_viewport(pane.pane_id(), Some(zone), dims);
        }

        if let Some(win) = self.window.as_ref() {
            win.invalidate();
        }
        Ok(())
    }

    fn scroll_by_page(&mut self, amount: f64, pane: &Arc<dyn Pane>) -> anyhow::Result<()> {
        let dims = pane.get_dimensions();
        let position = self
            .get_viewport(pane.pane_id())
            .unwrap_or(dims.physical_top) as f64
            + (amount * dims.viewport_rows as f64);
        self.set_viewport(pane.pane_id(), Some(position as isize), dims);
        if let Some(win) = self.window.as_ref() {
            win.invalidate();
        }
        Ok(())
    }

    fn scroll_by_current_event_wheel_delta(&mut self, pane: &Arc<dyn Pane>) -> anyhow::Result<()> {
        let Some(event) = self.current_mouse_event.clone() else {
            return Ok(());
        };
        let amount = match event.kind {
            MouseEventKind::VertWheel(amount) => -amount,
            _ => return Ok(()),
        };
        if crate::native_settings::scroll_mode() == crate::native_settings::NativeScrollMode::Smooth
        {
            // A device that reports pixels (a trackpad on macOS or Wayland,
            // a precision touchpad on Windows) moves the viewport by exactly
            // those pixels; a notched wheel glides the same distance over a
            // few frames instead of jumping.
            let cell_h = self.pane_cell_height(pane.pane_id());
            if let Some(delta) = event.precise_scroll_delta {
                if delta.y != 0.0 {
                    self.scroll_by_pixels(-delta.y, pane)?;
                }
                return Ok(());
            }
            if let Some(lines) = event.precise_wheel_lines {
                if lines != 0.0 {
                    self.scroll_by_pixels(-lines * cell_h, pane)?;
                }
                return Ok(());
            }
            if amount != 0 {
                self.start_scroll_glide(amount as f32 * cell_h, pane);
            }
            return Ok(());
        }
        if amount != 0 {
            self.scroll_by_line(amount.into(), pane)?;
        }
        Ok(())
    }

    fn scroll_by_line(&mut self, amount: isize, pane: &Arc<dyn Pane>) -> anyhow::Result<()> {
        let dims = pane.get_dimensions();
        let position = self
            .get_viewport(pane.pane_id())
            .unwrap_or(dims.physical_top)
            .saturating_add(amount);
        self.set_viewport(pane.pane_id(), Some(position), dims);
        if let Some(win) = self.window.as_ref() {
            win.invalidate();
        }
        Ok(())
    }

    /// The height of one cell of `pane_id` in physical pixels, honouring a
    /// per-pane font scale.
    pub(crate) fn pane_cell_height(&self, pane_id: PaneId) -> f32 {
        let global = self.render_metrics.cell_size.height as f32;
        let scale = self.pane_font_scale(pane_id);
        if scale.to_bits() == self.fonts.get_font_scale().to_bits() {
            return global.max(1.0);
        }
        self.pane_font_resources(scale)
            .map(|(_, metrics)| metrics.cell_size.height as f32)
            .unwrap_or(global)
            .max(1.0)
    }

    /// Move the viewport by `delta_px` physical pixels (positive is down,
    /// towards newer rows), carrying whole rows into `viewport` and keeping
    /// the remainder in `viewport_px`.
    fn scroll_by_pixels(&mut self, delta_px: f32, pane: &Arc<dyn Pane>) -> anyhow::Result<()> {
        let dims = pane.get_dimensions();
        let pane_id = pane.pane_id();
        let cell_h = self.pane_cell_height(pane_id);
        let (row, px) = {
            let state = self.pane_state(pane_id);
            (
                state.viewport.unwrap_or(dims.physical_top),
                state.viewport_px,
            )
        };
        let (row, px) = Self::normalize_scroll_px(row, px, delta_px, cell_h);
        self.set_viewport_px(pane_id, Some(row), px, dims);
        if let Some(win) = self.window.as_ref() {
            win.invalidate();
        }
        Ok(())
    }

    /// Queue `distance_px` of scrolling to be spread over the next frames.
    fn start_scroll_glide(&mut self, distance_px: f32, pane: &Arc<dyn Pane>) {
        {
            let mut state = self.pane_state(pane.pane_id());
            state.glide_remaining += distance_px;
            state.glide_last_tick = Some(Instant::now());
        }
        // The first step lands at once, so a single notch is felt without
        // waiting a frame; the rest follows from paint.
        self.advance_scroll_glide(pane);
    }

    /// Pay out a slice of a pending glide, sized by the time since the last
    /// slice, and ask for another frame while distance remains. Called once
    /// per painted frame for each pane; free when nothing is gliding.
    pub(crate) fn advance_scroll_glide(&mut self, pane: &Arc<dyn Pane>) {
        const RATE: f32 = 22.0;
        const SETTLED_PX: f32 = 0.5;
        let pane_id = pane.pane_id();
        let (remaining, last) = {
            let state = self.pane_state(pane_id);
            (state.glide_remaining, state.glide_last_tick)
        };
        if remaining == 0.0 {
            return;
        }
        let now = Instant::now();
        let dt = last
            .map(|last| now.saturating_duration_since(last).as_secs_f32())
            .unwrap_or(1.0 / 120.0)
            .clamp(1.0 / 240.0, 1.0 / 30.0);
        let step = if remaining.abs() <= SETTLED_PX {
            remaining
        } else {
            remaining * (1.0 - (-RATE * dt).exp())
        };
        {
            let mut state = self.pane_state(pane_id);
            state.glide_remaining -= step;
            state.glide_last_tick = Some(now);
            if state.glide_remaining.abs() <= SETTLED_PX {
                state.glide_remaining = 0.0;
                state.glide_last_tick = None;
            }
        }
        if let Err(err) = self.scroll_by_pixels(step, pane) {
            log::warn!("scroll glide: {err:#}");
        }
        if self.pane_state(pane_id).glide_remaining != 0.0 {
            self.update_next_frame_time(Some(now + Duration::from_millis(8)));
        }
    }

    /// Keep the pane's overlay scrollbar up: the pointer is on its thumb,
    /// or dragging it.
    pub(crate) fn reveal_scrollbar(&mut self, pane_id: PaneId) {
        self.pane_state(pane_id).scrollbar_visible_until =
            Some(Instant::now() + OVERLAY_SCROLLBAR_SHOW);
    }

    /// How visible the pane's overlay scrollbar is right now: 1 while it
    /// has more than the fade left, easing to 0 over the fade, 0 once it
    /// has expired or was never shown. Dragging the thumb pins it at 1.
    /// Also says when the next frame is due, so the caller can arm it.
    pub(crate) fn overlay_scrollbar_opacity(
        &self,
        pane_id: PaneId,
        now: Instant,
    ) -> (f32, Option<Instant>) {
        let dragging_this = self.dragging.as_ref().is_some_and(|(item, _)| {
            matches!(item.item_type, UIItemType::ScrollThumb(track) if track.pane_id == pane_id)
        });
        if dragging_this {
            return (1.0, None);
        }
        let Some(until) = self.pane_state(pane_id).scrollbar_visible_until else {
            return (0.0, None);
        };
        let Some(remaining) = until.checked_duration_since(now) else {
            return (0.0, None);
        };
        if remaining > OVERLAY_SCROLLBAR_FADE {
            return (1.0, Some(until - OVERLAY_SCROLLBAR_FADE));
        }
        let t = remaining.as_secs_f32() / OVERLAY_SCROLLBAR_FADE.as_secs_f32();
        (
            crate::ui::anim::Easing::Smooth.apply(t),
            Some(now + Duration::from_millis(16)),
        )
    }

    /// Carry `delta_px` into `(row, px)`: whole cells move the row, the
    /// remainder stays in `[0, cell_h)`. `floor` rather than `trunc`, so
    /// scrolling up through a row boundary is the mirror of scrolling down.
    pub(crate) fn normalize_scroll_px(
        row: StableRowIndex,
        px: f32,
        delta_px: f32,
        cell_h: f32,
    ) -> (StableRowIndex, f32) {
        if !(cell_h > 0.0) {
            return (row, 0.0);
        }
        let total = px + delta_px;
        let rows = (total / cell_h).floor();
        let mut px = total - rows * cell_h;
        let mut rows = rows as isize;
        // Float drift at a boundary must not leave px at cell_h.
        if px >= cell_h {
            px = 0.0;
            rows += 1;
        } else if px < 0.0 {
            px = 0.0;
        }
        (row.saturating_add(rows), px)
    }

    fn move_tab_relative(&mut self, delta: isize) -> anyhow::Result<()> {
        let mux = Mux::get();
        let window = mux
            .get_window(self.mux_window_id)
            .ok_or_else(|| anyhow!("no such window"))?;

        let max = window.len();
        ensure!(max > 0, "no more tabs");

        let active = window.get_active_idx();
        let tab = active as isize + delta;
        let tab = if tab < 0 {
            0usize
        } else if tab >= max as isize {
            max - 1
        } else {
            tab as usize
        };

        drop(window);
        self.move_tab(tab)
    }

    pub fn perform_key_assignment(
        &mut self,
        pane: &Arc<dyn Pane>,
        assignment: &KeyAssignment,
    ) -> anyhow::Result<PerformAssignmentResult> {
        use KeyAssignment::*;

        if let Some(modal) = self.get_modal() {
            if modal.perform_assignment(assignment, self) {
                return Ok(PerformAssignmentResult::Handled);
            }
        }

        if self.right_sidebar_has_text_focus() {
            match assignment {
                CopyTo(destination) => {
                    self.copy_right_sidebar_focused_input(*destination);
                    return Ok(PerformAssignmentResult::Handled);
                }
                PasteFrom(source) => {
                    self.paste_into_right_sidebar_from_clipboard(*source);
                    return Ok(PerformAssignmentResult::Handled);
                }
                ClearSelection => {
                    self.clear_right_sidebar_focused_input_selection();
                    return Ok(PerformAssignmentResult::Handled);
                }
                _ => {}
            }
        }

        if self.content_view_foreground() {
            match assignment {
                CopyTo(destination) => {
                    if let Some(text) = self
                        .active_content_view()
                        .and_then(|v| v.copy_text())
                        .filter(|text| !text.is_empty())
                    {
                        self.copy_to_clipboard(*destination, text);
                    } else if let Some(text) = self.right_sidebar_file_preview_selected_text() {
                        self.copy_to_clipboard(*destination, text);
                    }
                    return Ok(PerformAssignmentResult::Handled);
                }
                PasteFrom(source) => {
                    self.content_view_paste_from(*source);
                    return Ok(PerformAssignmentResult::Handled);
                }
                _ => {}
            }
        }

        if let CopyTo(destination) = assignment {
            let text = self.selection_text(pane);
            if !text.is_empty() {
                self.copy_to_clipboard(*destination, text);
                return Ok(PerformAssignmentResult::Handled);
            }
            if let Some(text) = self.right_sidebar_file_preview_selected_text() {
                self.copy_to_clipboard(*destination, text);
                return Ok(PerformAssignmentResult::Handled);
            }
        }

        match pane.perform_assignment(assignment) {
            PerformAssignmentResult::Unhandled => {}
            result => return Ok(result),
        }

        let window = self.window.as_ref().map(|w| w.clone());

        match assignment {
            ActivateKeyTable {
                name,
                timeout_milliseconds,
                replace_current,
                one_shot,
                until_unknown,
                prevent_fallback,
            } => {
                anyhow::ensure!(
                    self.input_map.has_table(name),
                    "ActivateKeyTable: no key_table named {}",
                    name
                );
                self.key_table_state.activate(KeyTableArgs {
                    name,
                    timeout_milliseconds: *timeout_milliseconds,
                    replace_current: *replace_current,
                    one_shot: *one_shot,
                    until_unknown: *until_unknown,
                    prevent_fallback: *prevent_fallback,
                });
                self.update_title();
            }
            PopKeyTable => {
                self.key_table_state.pop();
                self.update_title();
            }
            ClearKeyTableStack => {
                self.key_table_state.clear_stack();
                self.update_title();
            }
            Multiple(actions) => {
                for a in actions {
                    self.perform_key_assignment(pane, a)?;
                }
            }
            SpawnTab(spawn_where) => {
                self.spawn_tab(spawn_where);
            }
            SpawnTabToRight(spawn_where) => {
                self.spawn_tab_to_right(spawn_where);
            }
            SpawnWindow => {
                self.spawn_command(&SpawnCommand::default(), SpawnWhere::NewWindow);
            }
            SpawnCommandInNewTab(spawn) => {
                self.spawn_command(spawn, SpawnWhere::NewTab);
            }
            SpawnCommandInNewWindow(spawn) => {
                self.spawn_command(spawn, SpawnWhere::NewWindow);
            }
            SplitHorizontal(spawn) => {
                log::trace!("SplitHorizontal {:?}", spawn);
                self.restore_collapsed_panes_for_active_tab();
                self.spawn_command(
                    spawn,
                    SpawnWhere::SplitPane(SplitRequest {
                        direction: SplitDirection::Horizontal,
                        target_is_second: true,
                        size: MuxSplitSize::Percent(50),
                        top_level: false,
                    }),
                );
            }
            SplitVertical(spawn) => {
                log::trace!("SplitVertical {:?}", spawn);
                self.restore_collapsed_panes_for_active_tab();
                self.spawn_command(
                    spawn,
                    SpawnWhere::SplitPane(SplitRequest {
                        direction: SplitDirection::Vertical,
                        target_is_second: true,
                        size: MuxSplitSize::Percent(50),
                        top_level: false,
                    }),
                );
            }
            ToggleFullScreen => {
                self.window.as_ref().unwrap().toggle_fullscreen();
            }
            ToggleAlwaysOnTop => {
                let window = self.window.clone().unwrap();
                let current_level = self.window_state.as_window_level();

                match current_level {
                    WindowLevel::AlwaysOnTop => {
                        window.set_window_level(WindowLevel::Normal);
                    }
                    WindowLevel::AlwaysOnBottom | WindowLevel::Normal => {
                        window.set_window_level(WindowLevel::AlwaysOnTop);
                    }
                }
            }
            ToggleAlwaysOnBottom => {
                let window = self.window.clone().unwrap();
                let current_level = self.window_state.as_window_level();

                match current_level {
                    WindowLevel::AlwaysOnBottom => {
                        window.set_window_level(WindowLevel::Normal);
                    }
                    WindowLevel::AlwaysOnTop | WindowLevel::Normal => {
                        window.set_window_level(WindowLevel::AlwaysOnBottom);
                    }
                }
            }
            SetWindowLevel(level) => {
                let window = self.window.clone().unwrap();
                window.set_window_level(level.clone());
            }
            CopyTo(dest) => {
                let text = self.selection_text(pane);
                self.copy_to_clipboard(*dest, text);
            }
            CopyTextTo { text, destination } => {
                self.copy_to_clipboard(*destination, text.clone());
            }
            PasteFrom(source) => {
                self.paste_from_clipboard(pane, *source);
            }
            ActivateTabRelative(n) => {
                self.activate_tab_relative(*n, true)?;
            }
            ActivateTabRelativeNoWrap(n) => {
                self.activate_tab_relative(*n, false)?;
            }
            ActivateLastTab => self.activate_last_tab()?,
            DecreaseFontSize => self.decrease_font_size(),
            IncreaseFontSize => self.increase_font_size(),
            ResetFontSize => self.reset_font_size(),
            ResetFontAndWindowSize => {
                if let Some(w) = window.as_ref() {
                    self.reset_font_and_window_size(&w)?
                }
            }
            ActivateTab(n) => {
                self.activate_tab(*n)?;
            }
            ActivateWindow(n) => {
                self.activate_window(*n)?;
            }
            ActivateWindowRelative(n) => {
                self.activate_window_relative(*n, true)?;
            }
            ActivateWindowRelativeNoWrap(n) => {
                self.activate_window_relative(*n, false)?;
            }
            SendString(s) => pane.writer().write_all(s.as_bytes())?,
            SendKey(key) => {
                use keyevent::Key;
                let mods = key.mods;
                if let Key::Code(key) = self.win_key_code_to_termwiz_key_code(
                    &key.key.resolve(self.config.key_map_preference),
                ) {
                    pane.key_down(key, mods)?;
                }
            }
            Hide => {
                if let Some(w) = window.as_ref() {
                    w.hide();
                }
            }
            Show => {
                if let Some(w) = window.as_ref() {
                    w.show();
                }
            }
            CloseCurrentTab { confirm } => self.close_current_tab(*confirm),
            CloseCurrentPane { confirm } => self.close_current_pane(*confirm),
            Nop | DisableDefaultAssignment => {}
            ReloadConfiguration => config::reload(),
            MoveTab(n) => self.move_tab(*n)?,
            MoveTabRelative(n) => self.move_tab_relative(*n)?,
            ScrollByPage(n) => self.scroll_by_page(**n, pane)?,
            ScrollByLine(n) => self.scroll_by_line(*n, pane)?,
            ScrollByCurrentEventWheelDelta => self.scroll_by_current_event_wheel_delta(pane)?,
            ScrollToPrompt(n) => self.scroll_to_prompt(*n, pane)?,
            ScrollToTop => self.scroll_to_top(pane),
            ScrollToBottom => self.scroll_to_bottom(pane),
            ShowTabNavigator => self.show_tab_navigator(),
            PromptRenameTab => self.prompt_rename_current_tab(),
            PromptRenamePaneTab(pane_id) => self.prompt_rename_pane_tab(*pane_id),
            PromptRenameProject(project_id) => self.prompt_rename_project(project_id.clone()),
            RevealProjectInFolder(project_id) => self.reveal_project_in_folder(project_id),
            GrantProjectFolderAccess(project_id) => {
                self.grant_project_folder_access(project_id.clone())
            }
            OpenFileWith { path, app, label } => {
                let selected_app = crate::native_settings::NativeOpenWithApp {
                    id: app.clone(),
                    label: label.clone(),
                };
                self.right_sidebar_open_with_app = Some(selected_app.clone());
                if let Err(err) =
                    crate::native_settings::save_right_sidebar_open_with_app(selected_app)
                {
                    log::error!("failed to save Open With app selection: {err:#}");
                }
                wezterm_open_url::open_path_with_candidate(std::path::Path::new(path), app);
            }
            OpenFileWithSystemDefault(path) => {
                wezterm_open_url::open_url(path);
            }
            PickOpenFileWithApp(path) => {
                let file_path = path.clone();
                let Some(window) = self.window.as_ref().cloned() else {
                    return Ok(PerformAssignmentResult::Handled);
                };
                let notify_window = window.clone();
                window.pick_app_async(Box::new(move |app_path| {
                    if let Some(app_path) = app_path {
                        notify_window.notify(TermWindowNotif::Apply(Box::new(
                            move |term_window| {
                                term_window.finish_pick_open_with_app(&file_path, &app_path);
                            },
                        )));
                    }
                }));
            }
            RevealFileInFolder(path) => {
                wezterm_open_url::reveal_path(std::path::Path::new(path));
            }
            RenameSidebarFile(path) => {
                self.start_sidebar_file_rename(PathBuf::from(path));
            }
            TrashSidebarFile(path) => {
                self.trash_sidebar_file(Path::new(path));
            }
            CopyFilePathToClipboard(path) => {
                self.copy_to_clipboard(
                    config::keyassignment::ClipboardCopyDestination::Clipboard,
                    path.clone(),
                );
            }
            PromptRenameWorkspaceThread(thread_id) => {
                self.prompt_rename_workspace_thread(thread_id.clone())
            }
            PromptRenameSpace(space_id) => self.prompt_rename_space(space_id.clone()),
            CreateSpace => {
                let space_id = crate::workspace_threads::create_space(None);
                if let Some(window) = window.as_ref() {
                    self.switch_space(space_id.clone(), window);
                    self.prompt_rename_space(space_id);
                    window.invalidate();
                }
            }
            CreateSpaceOnDomain(domain_name) => {
                match crate::workspace_threads::create_space_on_domain(domain_name, None) {
                    Ok(space_id) => {
                        if let Some(window) = window.as_ref() {
                            self.switch_space(space_id.clone(), window);
                            self.prompt_rename_space(space_id);
                            window.invalidate();
                        }
                    }
                    Err(err) => {
                        log::warn!("failed to create Space on {domain_name}: {err:#}");
                    }
                }
            }
            SwitchSpace(space_id) => {
                if let Some(window) = window.as_ref() {
                    self.switch_space(space_id.clone(), window);
                }
            }
            DeleteSpace(space_id) => {
                // A remote Space belongs to its server; removing it here is a
                // disconnect, and it returns on the next connect.
                self.delete_space(
                    space_id,
                    window.as_ref(),
                    crate::workspace_threads::SpaceRemoval::Local,
                );
                if let Some(window) = window.as_ref() {
                    window.invalidate();
                }
            }
            DeleteSpaceEverywhere(space_id) => {
                self.delete_space(
                    space_id,
                    window.as_ref(),
                    crate::workspace_threads::SpaceRemoval::Everywhere,
                );
                if let Some(window) = window.as_ref() {
                    window.invalidate();
                }
            }
            DeleteSpaceAndRemoteSessions(space_id) => {
                self.delete_space_and_remote_sessions(space_id);
                if let Some(window) = window.as_ref() {
                    window.invalidate();
                }
            }
            CreateWorkspaceThread(project_id) => {
                if let Some(window) = window.as_ref() {
                    self.create_workspace_thread(project_id, window);
                }
            }
            ToggleWorkspaceThreadsCollapsed(project_id) => {
                crate::workspace_threads::toggle_project_threads_collapsed(project_id);
                if let Some(window) = window.as_ref() {
                    window.invalidate();
                }
            }
            RemoveProject(project_id) => {
                if let Some(removed) = crate::workspace_threads::remove_project(project_id) {
                    if removed.was_active {
                        if let (Some(next_thread_id), Some(window)) =
                            (removed.next_thread_id, window.as_ref())
                        {
                            self.activate_workspace_thread(next_thread_id, window);
                        }
                    }
                    let mux = Mux::get();
                    for workspace in removed.materialized_workspace_names {
                        for window_id in mux.iter_windows_in_workspace(&workspace) {
                            mux.kill_window(window_id);
                        }
                    }
                    if let Some(window) = window.as_ref() {
                        window.invalidate();
                    }
                }
            }
            ArchiveProject(project_id) => {
                self.start_archive_project(project_id);
            }
            UnarchiveProject(project_id) => {
                crate::workspace_threads::unarchive_project(project_id);
                if let Some(window) = window.as_ref() {
                    window.invalidate();
                }
            }
            ArchiveActiveProject => {
                if let Some(project_id) = crate::workspace_threads::active_project_id_for_space(
                    self.workspace_sidebar_space_id(),
                ) {
                    self.start_archive_project(&project_id);
                }
            }
            ToggleShowArchivedProjects => {
                self.workspace_sidebar_show_archived = !self.workspace_sidebar_show_archived;
                if let Some(window) = window.as_ref() {
                    window.invalidate();
                }
            }
            ConnectWorkspaceThread(thread_id) => {
                if let Some(window) = window.as_ref() {
                    self.connect_remote_thread(thread_id.clone(), window);
                }
            }
            DisconnectWorkspaceThread(thread_id) => {
                self.disconnect_workspace_thread(
                    thread_id,
                    window.as_ref().map(|w| w as &dyn WindowOps),
                );
            }
            ToggleWorkspaceThreadPinned(thread_id) => {
                crate::workspace_threads::toggle_thread_pinned(thread_id);
                if let Some(window) = window.as_ref() {
                    window.invalidate();
                }
            }
            DeleteWorkspaceThread(thread_id) => {
                self.end_workspace_thread(thread_id, window.as_ref().map(|w| w as &dyn WindowOps));
            }
            MarkWorkspaceThreadUnread(thread_id) => {
                crate::workspace_threads::mark_thread_unread(thread_id);
                if let Some(window) = window.as_ref() {
                    window.invalidate();
                }
            }
            ShowDebugOverlay => self.show_debug_overlay(),
            ShowLauncher => self.show_launcher(),
            ShowLauncherArgs(args) => {
                let title = args.title.clone().unwrap_or("Launcher".to_string());
                let args = LauncherActionArgs {
                    title: Some(title),
                    flags: args.flags,
                    help_text: args.help_text.clone(),
                    fuzzy_help_text: args.fuzzy_help_text.clone(),
                    alphabet: args.alphabet.clone(),
                };
                self.show_launcher_impl(args, 0);
            }
            HideApplication => {
                let con = Connection::get().expect("call on gui thread");
                con.hide_application();
            }
            QuitApplication => {
                let mux = Mux::get();
                let config = &self.config;
                log::info!("QuitApplication over here (window)");

                match config.window_close_confirmation {
                    WindowCloseConfirmation::NeverPrompt => {
                        let con = Connection::get().expect("call on gui thread");
                        con.terminate_message_loop();
                    }
                    WindowCloseConfirmation::AlwaysPrompt => {
                        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                            Some(tab) => tab,
                            None => anyhow::bail!("no active tab!?"),
                        };

                        let window = self.window.clone().unwrap();
                        let (overlay, future) = start_overlay(self, &tab, move |tab_id, term| {
                            confirm_quit_program(term, window, tab_id)
                        });
                        self.assign_overlay(tab.tab_id(), overlay);
                        promise::spawn::spawn(future).detach();
                    }
                }
            }
            SelectTextAtMouseCursor(mode) => self.select_text_at_mouse_cursor(*mode, pane),
            ExtendSelectionToMouseCursor(mode) => {
                self.extend_selection_at_mouse_cursor(*mode, pane)
            }
            ClearSelection => {
                self.clear_selection(pane);
            }
            StartWindowDrag => {
                self.window_drag_position = self.current_mouse_event.clone();
            }
            OpenLinkAtMouseCursor => {
                self.do_open_link_at_mouse_cursor(pane);
            }
            EmitEvent(name) => {
                self.emit_window_event(name, None);
            }
            CompleteSelectionOrOpenLinkAtMouseCursor(dest) => {
                // Note: the right sidebar file preview keeps its own selection,
                // which is copied only on explicit Cmd+C / the Copy button (see
                // the `CopyTo` handling above). We deliberately do not auto-copy
                // it on mouse release here — on macOS that would clobber the
                // system clipboard just from selecting/clicking.
                let text = self.selection_text(pane);
                if !text.is_empty() {
                    self.copy_to_clipboard(*dest, text);
                    let window = self.window.as_ref().unwrap();
                    window.invalidate();
                } else {
                    self.do_open_link_at_mouse_cursor(pane);
                }
            }
            CompleteSelection(dest) => {
                let text = self.selection_text(pane);
                if !text.is_empty() {
                    self.copy_to_clipboard(*dest, text);
                    let window = self.window.as_ref().unwrap();
                    window.invalidate();
                }
            }
            ClearScrollback(erase_mode) => {
                pane.erase_scrollback(*erase_mode);
                let window = self.window.as_ref().unwrap();
                window.invalidate();
            }
            Search(pattern) => {
                if let Some(pane) = self.get_active_pane_or_overlay() {
                    let mut replace_current = false;
                    if let Some(existing) = pane.downcast_ref::<CopyOverlay>() {
                        let mut params = existing.get_params();
                        params.editing_search = true;
                        if !pattern.is_empty() {
                            params.pattern = self.resolve_search_pattern(pattern.clone(), &pane);
                        }
                        existing.apply_params(params);
                        replace_current = true;
                    } else {
                        let search = CopyOverlay::with_pane(
                            self,
                            &pane,
                            CopyModeParams {
                                pattern: self.resolve_search_pattern(pattern.clone(), &pane),
                                editing_search: true,
                            },
                        )?;
                        self.assign_overlay_for_pane(pane.pane_id(), search);
                    }
                    self.pane_state(pane.pane_id())
                        .overlay
                        .as_mut()
                        .map(|overlay| {
                            overlay.key_table_state.activate(KeyTableArgs {
                                name: "search_mode",
                                timeout_milliseconds: None,
                                replace_current,
                                one_shot: false,
                                until_unknown: false,
                                prevent_fallback: false,
                            });
                        });
                }
            }
            QuickSelect => {
                if let Some(pane) = self.get_active_pane_no_overlay() {
                    let qa = QuickSelectOverlay::with_pane(
                        self,
                        &pane,
                        &QuickSelectArguments::default(),
                    );
                    self.assign_overlay_for_pane(pane.pane_id(), qa);
                }
            }
            QuickSelectArgs(args) => {
                if let Some(pane) = self.get_active_pane_no_overlay() {
                    let qa = QuickSelectOverlay::with_pane(self, &pane, args);
                    self.assign_overlay_for_pane(pane.pane_id(), qa);
                }
            }
            ActivateCopyMode => {
                if let Some(pane) = self.get_active_pane_or_overlay() {
                    let mut replace_current = false;
                    if let Some(existing) = pane.downcast_ref::<CopyOverlay>() {
                        let mut params = existing.get_params();
                        params.editing_search = false;
                        existing.apply_params(params);
                        replace_current = true;
                    } else {
                        let copy = CopyOverlay::with_pane(
                            self,
                            &pane,
                            CopyModeParams {
                                pattern: MuxPattern::default(),
                                editing_search: false,
                            },
                        )?;
                        self.assign_overlay_for_pane(pane.pane_id(), copy);
                    }
                    self.pane_state(pane.pane_id())
                        .overlay
                        .as_mut()
                        .map(|overlay| {
                            overlay.key_table_state.activate(KeyTableArgs {
                                name: "copy_mode",
                                timeout_milliseconds: None,
                                replace_current,
                                one_shot: false,
                                until_unknown: false,
                                prevent_fallback: false,
                            });
                        });
                }
            }
            AdjustPaneSize(direction, amount) => {
                if self.frontend_surface_blocked() {
                    return Ok(PerformAssignmentResult::Handled);
                }
                let mux = Mux::get();
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };

                let tab_id = tab.tab_id();

                if self.tab_state(tab_id).overlay.is_none() {
                    tab.adjust_pane_size(*direction, *amount);
                    self.preview_active_tab_geometry_now();
                    self.sync_active_tab_geometry_now();
                    self.persist_workspace_layout_after_mutation("pane size adjusted");
                }
            }
            ActivatePaneByIndex(index) => {
                if self.frontend_surface_blocked() {
                    return Ok(PerformAssignmentResult::Handled);
                }
                let mux = Mux::get();
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };

                let tab_id = tab.tab_id();

                if self.tab_state(tab_id).overlay.is_none() {
                    self.claim_frontend_viewport_for_interaction();
                    let panes = tab.iter_panes();
                    if panes.iter().position(|p| p.index == *index).is_some() {
                        tab.set_active_idx(*index);
                    }
                }
            }
            ActivatePaneDirection(direction) => {
                if self.frontend_surface_blocked() {
                    return Ok(PerformAssignmentResult::Handled);
                }
                let mux = Mux::get();
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };

                let tab_id = tab.tab_id();

                if self.tab_state(tab_id).overlay.is_none() {
                    self.claim_frontend_viewport_for_interaction();
                    tab.activate_pane_direction(*direction);
                }
            }
            TogglePaneZoomState => {
                if self.frontend_surface_blocked() {
                    return Ok(PerformAssignmentResult::Handled);
                }
                let mux = Mux::get();
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                mux::zoom_trace!(
                    "gui.gesture site=TogglePaneZoomState tab={} zoom={}",
                    tab.tab_id(),
                    tab.get_zoomed_pane().is_some()
                );
                self.claim_frontend_viewport_for_interaction();
                tab.toggle_zoom();
                // Zoom changes which panes occupy the tab root. Remote
                // mirrors are deliberately skipped by Tab::resize, so adopt
                // every resulting pane surface and publish one complete
                // viewport instead of waiting for a later window resize.
                self.sync_active_tab_geometry_now();
            }
            SetPaneZoomState(zoomed) => {
                if self.frontend_surface_blocked() {
                    return Ok(PerformAssignmentResult::Handled);
                }
                let mux = Mux::get();
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                mux::zoom_trace!(
                    "gui.gesture site=SetPaneZoomState tab={} want={zoomed} zoom={}",
                    tab.tab_id(),
                    tab.get_zoomed_pane().is_some()
                );
                self.claim_frontend_viewport_for_interaction();
                tab.set_zoomed(*zoomed);
                self.sync_active_tab_geometry_now();
            }
            SwitchWorkspaceRelative(delta) => {
                let mux = Mux::get();
                let workspace = mux.active_workspace();
                let workspaces = mux.iter_workspaces();
                let idx = workspaces.iter().position(|w| *w == workspace).unwrap_or(0);
                let new_idx = idx as isize + delta;
                let new_idx = if new_idx < 0 {
                    workspaces.len() as isize + new_idx
                } else {
                    new_idx
                };
                let new_idx = new_idx as usize % workspaces.len();
                if let Some(w) = workspaces.get(new_idx).cloned() {
                    self.adopt_workspace_in_this_window(&w);
                }
            }
            SwitchToWorkspace { name, spawn } => {
                let activity = crate::Activity::new();
                let mux = Mux::get();
                let name = name
                    .as_ref()
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| mux.generate_workspace_name());

                if mux.iter_windows_in_workspace(&name).is_empty() {
                    // Materialize a window in the target workspace, then adopt
                    // it into THIS window in place.
                    front_end().set_switching_workspaces(true);
                    mux.set_active_workspace(&name);
                    let spawn = spawn.as_ref().map(|s| s.clone()).unwrap_or_default();
                    let size = self.terminal_size;
                    let term_config = Arc::new(TermConfig::with_config(self.config.clone()));
                    let src_window_id = self.mux_window_id;
                    let reconcile_window = self.window.clone();
                    let adopt_workspace = name.clone();

                    promise::spawn::spawn(async move {
                        if let Err(err) = crate::spawn::spawn_command_internal(
                            spawn,
                            SpawnWhere::NewWindow,
                            size,
                            Some(src_window_id),
                            term_config,
                        )
                        .await
                        {
                            log::error!("Failed to spawn: {:#}", err);
                        }
                        crate::termwindow::mouseevent::adopt_workspace_into_window(
                            &reconcile_window,
                            &adopt_workspace,
                        );
                        front_end().set_switching_workspaces(false);
                        drop(activity);
                    })
                    .detach();
                } else {
                    self.adopt_workspace_in_this_window(&name);
                    drop(activity);
                }
            }
            DetachDomain(domain) => {
                let domain = Mux::get().resolve_spawn_tab_domain(Some(pane.pane_id()), domain)?;
                if domain.detachable() {
                    domain.detach()?;
                } else {
                    log::warn!("domain {} cannot be detached", domain.domain_name());
                }
            }
            AttachDomain(domain) => {
                let window = self.mux_window_id;
                let domain = domain.to_string();
                let dpi = self.dimensions.dpi as u32;

                promise::spawn::spawn(async move {
                    let mux = Mux::get();
                    let domain = mux
                        .get_domain_by_name(&domain)
                        .ok_or_else(|| anyhow!("{} is not a valid domain name", domain))?;
                    let ui =
                        mux::connui::ConnectionUI::with_params(mux::connui::ConnectionUIParams {
                            window_id: Some(window),
                            ..Default::default()
                        });
                    match crate::attach_domain_with_retry(
                        domain.clone(),
                        Some(window),
                        ui,
                        move || Mux::get().get_window(window).is_some(),
                        Some(std::time::Duration::from_secs(60)),
                    )
                    .await?
                    {
                        crate::AttachRetryOutcome::Attached => {}
                        crate::AttachRetryOutcome::Cancelled => {
                            return Result::<(), anyhow::Error>::Ok(())
                        }
                    }

                    let have_panes_in_domain = mux
                        .iter_panes()
                        .iter()
                        .any(|p| p.domain_id() == domain.domain_id());

                    if !have_panes_in_domain {
                        let config = config::configuration();
                        let _tab = domain
                            .spawn(
                                config.initial_size(
                                    dpi,
                                    Some(crate::cell_pixel_dims(&config, dpi as f64)?),
                                ),
                                None,
                                None,
                                window,
                            )
                            .await?;
                    }

                    Result::<(), anyhow::Error>::Ok(())
                })
                .detach();
            }
            CopyMode(_) => {
                // NOP here; handled by the overlay directly
            }
            RotatePanes(direction) => {
                if self.frontend_surface_blocked() {
                    return Ok(PerformAssignmentResult::Handled);
                }
                let mux = Mux::get();
                let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
                    Some(tab) => tab,
                    None => return Ok(PerformAssignmentResult::Handled),
                };
                match direction {
                    RotationDirection::Clockwise => tab.rotate_clockwise(),
                    RotationDirection::CounterClockwise => tab.rotate_counter_clockwise(),
                }
                self.preview_active_tab_geometry_now();
                self.sync_active_tab_geometry_now();
                self.persist_workspace_layout_after_mutation("panes rotated");
            }
            SplitPane(split) => {
                if self.frontend_surface_blocked() {
                    return Ok(PerformAssignmentResult::Handled);
                }
                log::trace!("SplitPane {:?}", split);
                self.restore_collapsed_panes_for_active_tab();
                self.spawn_command(
                    &split.command,
                    SpawnWhere::SplitPane(SplitRequest {
                        direction: match split.direction {
                            PaneDirection::Down | PaneDirection::Up => SplitDirection::Vertical,
                            PaneDirection::Left | PaneDirection::Right => {
                                SplitDirection::Horizontal
                            }
                            PaneDirection::Next | PaneDirection::Prev => {
                                log::error!(
                                    "Invalid direction {:?} for SplitPane",
                                    split.direction
                                );
                                return Ok(PerformAssignmentResult::Handled);
                            }
                        },
                        target_is_second: match split.direction {
                            PaneDirection::Down | PaneDirection::Right => true,
                            PaneDirection::Up | PaneDirection::Left => false,
                            PaneDirection::Next | PaneDirection::Prev => unreachable!(),
                        },
                        size: match split.size {
                            SplitSize::Percent(n) => MuxSplitSize::Percent(n),
                            SplitSize::Cells(n) => MuxSplitSize::Cells(n),
                        },
                        top_level: split.top_level,
                    }),
                );
            }
            PaneSelect(args) => {
                let modal = crate::termwindow::paneselect::PaneSelector::new(self, args);
                self.set_modal(Rc::new(modal));
            }
            CharSelect(args) => {
                let modal = crate::termwindow::charselect::CharSelector::new(self, args);
                self.set_modal(Rc::new(modal));
            }
            ResetTerminal => {
                pane.perform_actions(vec![termwiz::escape::Action::Esc(
                    termwiz::escape::Esc::Code(termwiz::escape::EscCode::FullReset),
                )]);
            }
            OpenUri(link) => {
                wezterm_open_url::open_url(link);
            }
            OpenSettings => {
                crate::settings_window::show_from(self.mux_window_id);
            }
            QuitAndStopSessionServer => {
                // The stop happens after the loop ends; the quit itself
                // goes through the same confirmation as any quit.
                crate::local_sessions::stop_server_at_exit();
                return self.perform_key_assignment(pane, &QuitApplication);
            }
            CheckForUpdates => {
                crate::settings_window::show_update_page();
            }
            OpenSshHosts => {
                self.toggle_ssh_hosts_view();
            }
            AddRemoteHost => {
                self.open_ssh_hosts_view_new_host();
            }
            ToggleLiveOverview => {
                self.toggle_live_overview_view();
            }
            ActivateCommandPalette => {
                self.toggle_command_palette();
            }
            SetColorScheme(name) => {
                self.set_color_scheme_override(name.clone());
            }
            ActivateWorkspaceThread {
                space_id,
                thread_id,
            } => {
                if let Some(window) = self.window.as_ref().cloned() {
                    self.switch_space_to_thread(space_id.clone(), Some(thread_id.clone()), &window);
                }
            }
            PromptInputLine(args) => self.show_prompt_input_line(args),
            InputSelector(args) => self.show_input_selector(args),
            Confirmation(args) => self.show_confirmation(args),
        };
        Ok(PerformAssignmentResult::Handled)
    }

    fn restore_collapsed_panes_for_active_tab(&mut self) {
        if self.collapsed_pane_layouts.is_empty() {
            return;
        }

        let Some(tab) = Mux::get().get_active_tab_for_window(self.mux_window_id) else {
            self.collapsed_pane_layouts.clear();
            return;
        };

        let layouts: Vec<_> = self
            .collapsed_pane_layouts
            .drain()
            .map(|(_, layout)| layout)
            .collect();
        for layout in layouts {
            if !tab.restore_collapsed_pane(layout) {
                self.collapsed_pane_layouts
                    .insert(layout.pane_stack_id, layout);
            }
        }
    }

    /// Reapply collapse state only to the tab whose geometry is being
    /// converged. Background tabs retain the server's canonical split tree
    /// until they are activated or explicitly claimed.
    pub(crate) fn reapply_collapsed_panes_for_tab(&mut self, tab_id: TabId) {
        let Some(tab) = Mux::get().get_tab(tab_id) else {
            return;
        };
        let min_cells = self.collapsed_pane_min_cells();

        // Adopt orphans first. `collapsed_pane_layouts` keys on stack ids
        // that are allocated per process and lives only in this window's
        // memory, so a pane that *arrives* at collapsed width -- session
        // restore, window adoption, workspace reconcile -- has no record
        // here. Without one it is painted as an ordinary terminal squeezed
        // to a few columns: the "strip of vertically wrapped prompt text on
        // the right edge" bug. A pane this narrow is not something anyone
        // can read or use, so claiming it as collapsed is strictly better
        // than drawing it raw.
        //
        // The threshold is deliberately looser than `min_cells` (2-3): the
        // orphan was squeezed by proportional resizes, not by the collapse
        // code, so it drifts -- observed at 6 cells. Ten columns is still
        // far below anything a terminal is usable at, and legitimate narrow
        // threads (~20 cells) stay untouched.
        let threshold = (min_cells * 3).max(10);
        let orphans: Vec<usize> = tab
            .iter_panes_ignoring_zoom()
            .into_iter()
            .filter(|pos| {
                // No direction test: side-by-side panes report their split as
                // Horizontal and collapse into a vertical strip. Whether a
                // given pane can collapse at all is `collapse_pane_by_index`'s
                // call -- it returns None when it cannot.
                !pos.is_active
                    && pos.width <= threshold
                    && !self.collapsed_pane_layouts.contains_key(&pos.pane_stack_id)
            })
            .map(|pos| pos.index)
            .collect();
        for index in orphans {
            if let Some(layout) = tab.collapse_pane_by_index(index, min_cells) {
                log::info!(
                    "adopted orphan collapsed pane stack {:?} (index {index})",
                    layout.pane_stack_id
                );
                self.collapsed_pane_layouts
                    .insert(layout.pane_stack_id, layout);
            }
        }

        if self.collapsed_pane_layouts.is_empty() {
            return;
        }
        let layouts: Vec<_> = self.collapsed_pane_layouts.values().copied().collect();
        for layout in layouts {
            let contains_stack = tab
                .iter_panes_ignoring_zoom()
                .into_iter()
                .any(|pane| pane.pane_stack_id == layout.pane_stack_id);
            if contains_stack {
                tab.reapply_collapsed_pane(layout, min_cells);
            }
        }
    }

    fn do_open_link_at_mouse_cursor(&self, pane: &Arc<dyn Pane>) {
        // They clicked on a link, so let's open it!
        // We need to ensure that we spawn the `open` call outside of the context
        // of our window loop; on Windows it can cause a panic due to
        // triggering our WndProc recursively.
        // We get that assurance for free as part of the async dispatch that we
        // perform below; here we allow the user to define an `open-uri` event
        // handler that can bypass the normal `open_url` functionality.
        if let Some(link) = self.current_highlight.as_ref().cloned() {
            let window = GuiWin::new(self);
            let pane = MuxPane(pane.pane_id());

            async fn open_uri(
                lua: Option<Rc<mlua::Lua>>,
                window: GuiWin,
                pane: MuxPane,
                link: String,
            ) -> anyhow::Result<()> {
                let default_click = match lua {
                    Some(lua) => {
                        let args = lua.pack_multi((window, pane, link.clone()))?;
                        config::lua::emit_event(&lua, ("open-uri".to_string(), args))
                            .await
                            .map_err(|e| {
                                log::error!("while processing open-uri event: {:#}", e);
                                e
                            })?
                    }
                    None => true,
                };
                if default_click {
                    log::info!("clicking {}", link);
                    wezterm_open_url::open_url(&link);
                }
                Ok(())
            }

            promise::spawn::spawn(config::with_lua_config_on_main_thread(move |lua| {
                open_uri(lua, window, pane, link.uri().to_string())
            }))
            .detach();
        }
    }
    fn close_current_pane(&mut self, confirm: bool) {
        let mux_window_id = self.mux_window_id;
        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(mux_window_id) {
            Some(tab) => tab,
            None => return,
        };
        let pane = match tab.get_active_pane() {
            Some(p) => p,
            None => return,
        };

        self.close_pane(pane, confirm);
    }

    fn close_pane(&mut self, pane: Arc<dyn Pane>, confirm: bool) {
        let mux_window_id = self.mux_window_id;
        let mux = Mux::get();
        let pane_id = pane.pane_id();
        if confirm && !pane.can_close_without_prompting(CloseReason::Pane) {
            let window = self.window.clone().unwrap();
            let (overlay, future) = start_overlay_pane(self, &pane, move |pane_id, term| {
                confirm_close_pane(pane_id, term, mux_window_id, window)
            });
            self.assign_overlay_for_pane(pane_id, overlay);
            promise::spawn::spawn(future).detach();
        } else {
            mux.remove_pane(pane_id);
        }
    }

    fn close_specific_tab(&mut self, tab_idx: usize, confirm: bool) {
        let mux = Mux::get();
        let mux_window_id = self.mux_window_id;
        let mux_window = match mux.get_window(mux_window_id) {
            Some(w) => w,
            None => return,
        };

        let tab = match mux_window.get_by_idx(tab_idx) {
            Some(tab) => Arc::clone(tab),
            None => return,
        };
        drop(mux_window);

        let tab_id = tab.tab_id();
        if confirm && !tab.can_close_without_prompting(CloseReason::Tab) {
            if self.activate_tab(tab_idx as isize).is_err() {
                return;
            }

            let window = self.window.clone().unwrap();
            let (overlay, future) = start_overlay(self, &tab, move |tab_id, term| {
                confirm_close_tab(tab_id, term, mux_window_id, window)
            });
            self.assign_overlay(tab_id, overlay);
            promise::spawn::spawn(future).detach();
        } else {
            mux.remove_tab(tab_id);
        }
    }

    fn close_current_tab(&mut self, confirm: bool) {
        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };
        let tab_id = tab.tab_id();
        let mux_window_id = self.mux_window_id;
        if confirm && !tab.can_close_without_prompting(CloseReason::Tab) {
            let window = self.window.clone().unwrap();
            let (overlay, future) = start_overlay(self, &tab, move |tab_id, term| {
                confirm_close_tab(tab_id, term, mux_window_id, window)
            });
            self.assign_overlay(tab_id, overlay);
            promise::spawn::spawn(future).detach();
        } else {
            mux.remove_tab(tab_id);
        }
    }

    pub fn pane_state(&self, pane_id: PaneId) -> RefMut<'_, PaneState> {
        RefMut::map(self.pane_state.borrow_mut(), |state| {
            state.entry(pane_id).or_insert_with(PaneState::default)
        })
    }

    pub fn pane_font_scale(&self, pane_id: PaneId) -> f64 {
        self.pane_state
            .borrow()
            .get(&pane_id)
            .and_then(|state| state.font_scale)
            .unwrap_or_else(|| self.fonts.get_font_scale())
    }

    pub fn pane_font_resources(
        &self,
        font_scale: f64,
    ) -> anyhow::Result<(Rc<FontConfiguration>, RenderMetrics)> {
        let key = PaneFontKey {
            font_scale_bits: font_scale.to_bits(),
            dpi: self.dimensions.dpi,
            config_generation: self.config.generation(),
        };

        let tick = self.pane_font_cache_tick.get().wrapping_add(1);
        self.pane_font_cache_tick.set(tick);

        if let Some(entry) = self.pane_font_cache.borrow().get(&key) {
            entry.last_used.set(tick);
            return Ok((Rc::clone(&entry.fonts), entry.render_metrics));
        }

        let build_started = crate::perf::now();
        let fonts = Rc::new(FontConfiguration::new(
            Some(self.config.clone()),
            self.dimensions.dpi,
        )?);
        fonts.change_scaling(font_scale, self.dimensions.dpi);
        let render_metrics = RenderMetrics::new(&fonts)?;

        {
            let mut cache = self.pane_font_cache.borrow_mut();
            // Each entry is a whole FontConfiguration (fonts, shaper state,
            // atlas-resident glyphs); unbounded, the map only ever grows as
            // scrolling and resizing walk new scale buckets.
            const PANE_FONT_CACHE_CAP: usize = 16;
            while cache.len() >= PANE_FONT_CACHE_CAP {
                let Some(oldest) = cache
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_used.get())
                    .map(|(key, _)| *key)
                else {
                    break;
                };
                cache.remove(&oldest);
            }
            cache.insert(
                key,
                PaneFontEntry {
                    fonts: Rc::clone(&fonts),
                    render_metrics,
                    last_used: Cell::new(tick),
                },
            );
        }
        crate::perf::accum("font_config_build", build_started);

        Ok((fonts, render_metrics))
    }

    pub fn tab_state(&self, tab_id: TabId) -> RefMut<'_, TabState> {
        RefMut::map(self.tab_state.borrow_mut(), |state| {
            state.entry(tab_id).or_insert_with(TabState::default)
        })
    }

    /// Resize overlays to match their corresponding tab/pane dimensions
    pub fn resize_overlays(&self) {
        let mux = Mux::get();
        for (_, state) in self.tab_state.borrow().iter() {
            if let Some(overlay) = state.overlay.as_ref().map(|o| &o.pane) {
                overlay.resize(self.terminal_size).ok();
            }
        }
        for (pane_id, state) in self.pane_state.borrow().iter() {
            if let Some(overlay) = state.overlay.as_ref().map(|o| &o.pane) {
                if let Some(pane) = mux.get_pane(*pane_id) {
                    let dims = pane.get_dimensions();
                    overlay
                        .resize(TerminalSize {
                            cols: dims.cols,
                            rows: dims.viewport_rows,
                            dpi: self.terminal_size.dpi,
                            pixel_height: (self.terminal_size.pixel_height
                                / self.terminal_size.rows)
                                * dims.viewport_rows,
                            pixel_width: (self.terminal_size.pixel_width / self.terminal_size.cols)
                                * dims.cols,
                        })
                        .ok();
                }
            }
        }
    }

    pub fn get_viewport(&self, pane_id: PaneId) -> Option<StableRowIndex> {
        self.pane_state(pane_id).viewport
    }

    /// If a saved viewport has fallen below the retained scrollback — the
    /// trim passed it, the app erased its scrollback, or a rewrap
    /// renumbered rows beyond what the anchor could compensate — produce
    /// the corrected position: the oldest retained row, or follow-bottom
    /// when no scrollback remains at all. `None` means the viewport is
    /// still valid as-is. Kept as a pure function so the policy is
    /// testable without a TermWindow.
    pub(crate) fn normalize_stale_viewport(
        viewport: Option<StableRowIndex>,
        dims: &RenderableDimensions,
    ) -> Option<Option<StableRowIndex>> {
        match viewport {
            Some(v) if v < dims.scrollback_top => {
                Some(if dims.scrollback_top >= dims.physical_top {
                    None
                } else {
                    Some(dims.scrollback_top)
                })
            }
            _ => None,
        }
    }

    /// Row-granular viewport change: every caller that thinks in rows
    /// (keys, prompt jumps, the copy overlay, stale-viewport fixes) lands on
    /// a whole row, so any smooth-scroll remainder is dropped.
    pub fn set_viewport(
        &mut self,
        pane_id: PaneId,
        position: Option<StableRowIndex>,
        dims: RenderableDimensions,
    ) {
        self.set_viewport_px(pane_id, position, 0.0, dims)
    }

    /// The pixel remainder of a smooth scroll for `pane_id`; 0 unless the
    /// viewport sits between two rows.
    pub fn get_viewport_px(&self, pane_id: PaneId) -> f32 {
        self.pane_state(pane_id).viewport_px
    }

    /// The one place the viewport is written. `px` is how far `position`
    /// is cut off at its top; it only survives when `position` was taken
    /// as given -- a clamp at either end of the scrollback, or following
    /// the bottom, always lands on a whole row.
    pub fn set_viewport_px(
        &mut self,
        pane_id: PaneId,
        position: Option<StableRowIndex>,
        px: f32,
        dims: RenderableDimensions,
    ) {
        let pos = match position {
            Some(pos) => {
                // Drop out of scrolling mode if we're off the bottom
                if pos >= dims.physical_top {
                    None
                } else {
                    Some(pos.max(dims.scrollback_top))
                }
            }
            None => None,
        };
        let px = if pos.is_some() && pos == position && px.is_finite() {
            px.max(0.0)
        } else {
            0.0
        };

        let mut state = self.pane_state(pane_id);
        let moved = px != state.viewport_px || pos != state.viewport;
        if px != state.viewport_px {
            state.viewport_px = px;
        }
        if pos.is_none() || pos != position {
            // Nothing left to glide towards past the end.
            state.glide_remaining = 0.0;
            state.glide_last_tick = None;
        }
        if moved {
            // The overlay scrollbar shows where the view went.
            state.scrollbar_visible_until = Some(Instant::now() + OVERLAY_SCROLLBAR_SHOW);
        }
        if pos != state.viewport {
            state.viewport = pos;

            // This is a bit gross.  If we add other overlays that need this information,
            // this should get extracted out into a trait
            if let Some(overlay) = state.overlay.as_ref() {
                if let Some(copy) = overlay.pane.downcast_ref::<CopyOverlay>() {
                    copy.viewport_changed(pos);
                } else if let Some(qs) = overlay.pane.downcast_ref::<QuickSelectOverlay>() {
                    qs.viewport_changed(pos);
                }
            }
        }
        self.window.as_ref().unwrap().invalidate();
    }

    fn maybe_scroll_to_bottom_for_input(&mut self, pane: &Arc<dyn Pane>) {
        if self.config.scroll_to_bottom_on_input {
            self.scroll_to_bottom(pane);
        }
    }

    fn scroll_to_top(&mut self, pane: &Arc<dyn Pane>) {
        let dims = pane.get_dimensions();
        self.set_viewport(pane.pane_id(), Some(dims.scrollback_top), dims);
    }

    fn scroll_to_bottom(&mut self, pane: &Arc<dyn Pane>) {
        let mut state = self.pane_state(pane.pane_id());
        state.viewport = None;
        state.viewport_px = 0.0;
        state.glide_remaining = 0.0;
        state.glide_last_tick = None;
    }

    fn get_active_pane_no_overlay(&self) -> Option<Arc<dyn Pane>> {
        let mux = Mux::get();
        mux.get_active_tab_for_window(self.mux_window_id)
            .and_then(|tab| tab.get_active_pane())
    }

    /// Returns a Pane that we can interact with; this will typically be
    /// the active tab for the window, but if the window has a tab-wide
    /// overlay (such as the launcher / tab navigator),
    /// then that will be returned instead.  Otherwise, if the pane has
    /// an active overlay (such as search or copy mode) then that will
    /// be returned.
    pub fn get_active_pane_or_overlay(&self) -> Option<Arc<dyn Pane>> {
        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return None,
        };

        let tab_id = tab.tab_id();

        if let Some(tab_overlay) = self
            .tab_state(tab_id)
            .overlay
            .as_ref()
            .map(|overlay| overlay.pane.clone())
        {
            Some(tab_overlay)
        } else {
            let pane = tab.get_active_pane()?;
            let pane_id = pane.pane_id();
            self.pane_state(pane_id)
                .overlay
                .as_ref()
                .map(|overlay| overlay.pane.clone())
                .or_else(|| Some(pane))
        }
    }

    fn get_splits(&mut self) -> Vec<PositionedSplit> {
        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return vec![],
        };

        let tab_id = tab.tab_id();

        if self.tab_state(tab_id).overlay.is_some() {
            vec![]
        } else {
            tab.iter_splits()
        }
    }

    fn pos_pane_to_pane_info(pos: &PositionedPane) -> PaneInformation {
        PaneInformation {
            pane_id: pos.pane.pane_id(),
            pane_index: pos.index,
            is_active: pos.is_active,
            is_zoomed: pos.is_zoomed,
            has_unseen_output: pos.pane.has_unseen_output(),
            left: pos.left,
            top: pos.top,
            width: pos.width,
            height: pos.height,
            pixel_width: pos.pixel_width,
            pixel_height: pos.pixel_height,
            title: pos.pane.get_title(),
            user_vars: pos.pane.copy_user_vars(),
            progress: pos.pane.get_progress(),
        }
    }

    fn get_tab_information(&mut self) -> Vec<TabInformation> {
        let mux = Mux::get();
        let window = match mux.get_window(self.mux_window_id) {
            Some(window) => window,
            _ => return vec![],
        };
        let tab_index = window.get_active_idx();

        window
            .iter()
            .enumerate()
            .map(|(idx, tab)| {
                let panes = self.get_pos_panes_for_tab(tab);
                let tab_id = tab.tab_id();
                let tab_title = self
                    .inline_window_tab_rename_title(tab_id)
                    .unwrap_or_else(|| tab.get_title());

                TabInformation {
                    tab_index: idx,
                    tab_id,
                    is_active: tab_index == idx,
                    is_last_active: window
                        .get_last_active_idx()
                        .map(|last_active| last_active == idx)
                        .unwrap_or(false),
                    window_id: self.mux_window_id,
                    tab_title,
                    active_pane: panes
                        .iter()
                        .find(|p| p.is_active)
                        .map(Self::pos_pane_to_pane_info),
                }
            })
            .collect()
    }

    fn get_pane_information(&self) -> Vec<PaneInformation> {
        self.get_panes_to_render()
            .iter()
            .map(Self::pos_pane_to_pane_info)
            .collect()
    }

    fn get_pos_panes_for_tab(&self, tab: &Arc<Tab>) -> Vec<PositionedPane> {
        let tab_id = tab.tab_id();

        if let Some(pane) = self
            .tab_state(tab_id)
            .overlay
            .as_ref()
            .map(|overlay| overlay.pane.clone())
        {
            let size = tab.get_size();
            vec![PositionedPane {
                pane_stack_id: pane.pane_id(),
                index: 0,
                is_active: true,
                is_zoomed: false,
                left: 0,
                top: 0,
                width: size.cols as _,
                height: size.rows as _,
                pixel_width: size.cols as usize * self.render_metrics.cell_size.width as usize,
                pixel_height: size.rows as usize * self.render_metrics.cell_size.height as usize,
                pane,
            }]
        } else {
            let mut panes = tab.iter_panes();
            for p in &mut panes {
                if let Some(overlay) = self.pane_state(p.pane.pane_id()).overlay.as_ref() {
                    p.pane = Arc::clone(&overlay.pane);
                }
            }
            panes
        }
    }

    fn get_panes_to_render(&self) -> Vec<PositionedPane> {
        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return vec![],
        };

        self.get_pos_panes_for_tab(&tab)
    }

    /// if pane_id.is_none(), removes any overlay for the specified tab.
    /// Otherwise: if the overlay is the specified pane for that tab, remove it.
    fn cancel_overlay_for_tab(&mut self, tab_id: TabId, pane_id: Option<PaneId>) {
        if pane_id.is_some() {
            let current = self
                .tab_state(tab_id)
                .overlay
                .as_ref()
                .map(|o| o.pane.pane_id());
            if current != pane_id {
                return;
            }
        }
        if let Some(overlay) = self.tab_state(tab_id).overlay.take() {
            Mux::get().remove_pane(overlay.pane.pane_id());
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub fn schedule_cancel_overlay(window: Window, tab_id: TabId, pane_id: Option<PaneId>) {
        window.notify(TermWindowNotif::CancelOverlayForTab { tab_id, pane_id });
    }

    fn cancel_overlay_for_pane(&mut self, pane_id: PaneId) {
        if let Some(overlay) = self.pane_state(pane_id).overlay.take() {
            // Ungh, when I built the CopyOverlay, its pane doesn't get
            // added to the mux and instead it reports the overlaid
            // pane id.  Take care to avoid killing ourselves off
            // when closing the CopyOverlay
            if pane_id != overlay.pane.pane_id() {
                Mux::get().remove_pane(overlay.pane.pane_id());
            }
        }
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    pub fn schedule_cancel_overlay_for_pane(window: Window, pane_id: PaneId) {
        window.notify(TermWindowNotif::CancelOverlayForPane(pane_id));
    }

    pub fn assign_overlay_for_pane(&mut self, pane_id: PaneId, pane: Arc<dyn Pane>) {
        self.cancel_overlay_for_pane(pane_id);
        self.pane_state(pane_id).overlay.replace(OverlayState {
            pane,
            key_table_state: KeyTableState::default(),
        });
        // Overlays are exempt from the unfocused repaint throttle (the
        // render watchdog cannot backstop them); an armed trailing-edge
        // timer from before the overlay must not swallow their output
        // events, so disarm it here at the transition.
        self.unfocused_invalidate_due = None;
        self.update_title();
    }

    pub fn assign_overlay(&mut self, tab_id: TabId, overlay: Arc<dyn Pane>) {
        self.cancel_overlay_for_tab(tab_id, None);
        self.tab_state(tab_id).overlay.replace(OverlayState {
            pane: overlay,
            key_table_state: KeyTableState::default(),
        });
        // See assign_overlay_for_pane: overlays must not inherit an
        // armed throttle timer.
        self.unfocused_invalidate_due = None;
        self.update_title();
    }

    fn resolve_search_pattern(&self, pattern: Pattern, pane: &Arc<dyn Pane>) -> MuxPattern {
        match pattern {
            Pattern::CaseSensitiveString(s) => MuxPattern::CaseSensitiveString(s),
            Pattern::CaseInSensitiveString(s) => MuxPattern::CaseInSensitiveString(s),
            Pattern::Regex(s) => MuxPattern::Regex(s),
            Pattern::CurrentSelectionOrEmptyString => {
                let text = self.selection_text(pane);
                let first_line = text
                    .lines()
                    .next()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                MuxPattern::CaseSensitiveString(first_line)
            }
        }
    }
}

impl TermWindow {
    /// The IME composing status as the TERMINAL should see it. While a
    /// sidebar text input (the Note editor, snippet fields, file filter)
    /// holds keyboard focus, composed text is routed there — the terminal
    /// cursor must not render a duplicate preedit overlay at the prompt.
    pub(crate) fn terminal_dead_key_status(&self) -> &DeadKeyStatus {
        static NONE: DeadKeyStatus = DeadKeyStatus::None;
        if self.right_sidebar_has_text_focus() {
            &NONE
        } else {
            &self.dead_key_status
        }
    }

    /// Publish this window's per-domain shaping-cache gauges in one batch
    /// (single diagnostics lock). Called unthrottled after cache clears and
    /// idle releases so the panel never shows stale non-zero values.
    pub(crate) fn publish_ui_shape_cache_diagnostics(&self) {
        if !crate::input_diagnostics::enabled() {
            return;
        }
        self.last_ui_shape_diagnostics_publish
            .set(Some(Instant::now()));
        let caches = self.ui_shape_caches.borrow();
        let mut values: Vec<(&'static str, u64)> = Vec::with_capacity(27);
        for domain in [
            crate::shapecache::UiTextDomain::Chrome,
            crate::shapecache::UiTextDomain::Note,
            crate::shapecache::UiTextDomain::FilePreview,
        ] {
            let cache = caches.domain(domain);
            let stats = cache.stats();
            let names = domain.gauge_names();
            values.push((names.len, cache.len() as u64));
            values.push((names.bytes, cache.total_weight() as u64));
            values.push((names.cap, cache.cap() as u64));
            values.push((names.budget, cache.byte_budget() as u64));
            values.push((names.hits, stats.hits));
            values.push((names.misses, stats.misses));
            values.push((names.rejected_oversize, stats.rejected_oversize));
            values.push((names.evicted_entries, stats.evicted_entries));
            values.push((names.evicted_bytes, stats.evicted_bytes));
        }
        crate::input_diagnostics::set_gauges_for_source(self.space_owner_id, &values);
    }

    /// Throttled variant for the per-frame paint path.
    pub(crate) fn publish_ui_shape_cache_diagnostics_throttled(&self) {
        const PUBLISH_INTERVAL: Duration = Duration::from_millis(500);
        if !crate::input_diagnostics::enabled() {
            return;
        }
        if self
            .last_ui_shape_diagnostics_publish
            .get()
            .is_some_and(|last| last.elapsed() < PUBLISH_INTERVAL)
        {
            return;
        }
        self.publish_ui_shape_cache_diagnostics();
    }
}

impl Drop for TermWindow {
    fn drop(&mut self) {
        // WindowEvent::Destroyed normally releases this claim, but a mux
        // window can disappear first (for example during an asynchronous
        // remote attach) and tear down TermWindow without delivering that
        // native event. Release is idempotent and prevents a vanished window
        // from leaving its Space permanently marked as occupied.
        crate::workspace_threads::release_window_space(self.space_owner_id);
        crate::input_diagnostics::remove_gauges_for_source(self.space_owner_id);
        self.clear_gui_recovery_intent();
        gpu_debug(format!(
            "drop main_window backend={} size={}x{} dpi={}",
            if self.webgpu.is_some() {
                "WebGpu"
            } else if self.gl.is_some() {
                "OpenGL"
            } else {
                "none"
            },
            self.dimensions.pixel_width,
            self.dimensions.pixel_height,
            self.dimensions.dpi
        ));
        self.clear_all_overlays();
        if let Some(window) = self.window.take() {
            if let Some(fe) = try_front_end() {
                fe.forget_known_window(&window);
            }
        }
    }
}

/// How long a window must stay fully occluded before its lazily-rebuilt
/// caches are released. Long enough to survive a glance at another Space
/// or a brief cover-up; matches the sidebar's idle-release precedent.
const OCCLUSION_RELEASE_SECS: u64 = 30;

fn occlusion_release_due(occluded_for: Option<Duration>, already_released: bool) -> bool {
    match occluded_for {
        Some(elapsed) => {
            !already_released && elapsed >= Duration::from_secs(OCCLUSION_RELEASE_SECS)
        }
        None => false,
    }
}

#[cfg(test)]
mod occlusion_release_tests {
    use super::{occlusion_release_due, OCCLUSION_RELEASE_SECS};
    use std::time::Duration;

    fn past_grace() -> Duration {
        Duration::from_secs(OCCLUSION_RELEASE_SECS + 1)
    }

    #[test]
    fn a_window_hidden_past_the_grace_period_releases() {
        assert!(occlusion_release_due(Some(past_grace()), false));
    }

    #[test]
    fn a_visible_window_never_releases() {
        assert!(!occlusion_release_due(None, false));
    }

    #[test]
    fn the_grace_period_is_respected() {
        assert!(!occlusion_release_due(Some(Duration::from_secs(5)), false));
    }

    #[test]
    fn an_episode_releases_once_until_marked_dirty_again() {
        assert!(!occlusion_release_due(Some(past_grace()), true));
    }
}

#[cfg(test)]
mod scroll_px_tests {
    use super::TermWindow;

    const CELL: f32 = 20.0;

    #[test]
    fn whole_cells_move_the_row_and_the_remainder_stays_in_range() {
        assert_eq!(TermWindow::normalize_scroll_px(10, 0.0, 45.0, CELL), (12, 5.0));
        assert_eq!(TermWindow::normalize_scroll_px(10, 5.0, 15.0, CELL), (11, 0.0));
        assert_eq!(TermWindow::normalize_scroll_px(10, 5.0, 40.0, CELL), (12, 5.0));
        let (_, px) = TermWindow::normalize_scroll_px(10, 19.9, 0.2, CELL);
        assert!(px >= 0.0 && px < CELL, "px={px}");
    }

    #[test]
    fn scrolling_up_mirrors_scrolling_down() {
        // Five pixels into row 10, then twelve pixels up: seven pixels
        // short of row 10's top, i.e. thirteen pixels into row 9.
        assert_eq!(TermWindow::normalize_scroll_px(10, 5.0, -12.0, CELL), (9, 13.0));
        // Back down by the same amount lands where it started.
        assert_eq!(TermWindow::normalize_scroll_px(9, 13.0, 12.0, CELL), (10, 5.0));
        // Exactly one row up from a row boundary is the previous boundary.
        assert_eq!(TermWindow::normalize_scroll_px(10, 0.0, -20.0, CELL), (9, 0.0));
    }

    #[test]
    fn a_zero_cell_height_never_produces_a_remainder() {
        assert_eq!(TermWindow::normalize_scroll_px(10, 3.0, 50.0, 0.0), (10, 0.0));
    }
}
