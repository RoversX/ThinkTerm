use async_trait::async_trait;
use bitflags::bitflags;
use config::window::WindowLevel;
use config::{ConfigHandle, Dimension, GeometryOrigin};
use promise::Future;
use std::any::Any;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use thiserror::Error;
use url::Url;
pub mod bitmaps;
pub use wezterm_color_types as color;
mod configuration;
pub mod connection;
pub mod os;
pub mod screen;
mod spawn;

pub use raw_window_handle;

#[cfg(target_os = "macos")]
pub(crate) const DEFAULT_DPI: f64 = 72.0;
#[cfg(not(target_os = "macos"))]
pub(crate) const DEFAULT_DPI: f64 = 96.0;

pub fn default_dpi() -> f64 {
    match Connection::get() {
        Some(conn) => conn.default_dpi(),
        None => DEFAULT_DPI,
    }
}

mod egl;

pub use bitmaps::{BitmapImage, Image};
pub use connection::*;
pub use glium;
pub use os::*;
pub use wezterm_input_types::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clipboard {
    Clipboard,
    PrimarySelection,
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::Clipboard
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimensions {
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub dpi: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContextMenuAction {
    KeyAssignment(config::keyassignment::KeyAssignment),
    ApplicationAction(u64),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextCheckCapabilities {
    pub spelling: bool,
    pub suggestions: bool,
    pub ignore: bool,
    pub learn: bool,
}

#[derive(Debug, Clone)]
pub struct TextCheckRequest {
    pub document_id: String,
    pub request_id: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextCheckIssue {
    /// UTF-8 byte range in `TextCheckRequest::text`.
    pub range: Range<usize>,
    pub suggestions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TextCheckResponse {
    pub request_id: u64,
    pub issues: Vec<TextCheckIssue>,
}

#[derive(Debug, Clone)]
pub struct NativeTextHit {
    pub rect: Rect,
    /// UTF-8 byte offset in `NativeTextInputSnapshot::text`.
    pub byte: usize,
}

#[derive(Debug, Clone)]
pub struct NativeTextInputSnapshot {
    pub token: u64,
    pub revision: u64,
    pub source_base: usize,
    pub text: String,
    pub selection: Range<usize>,
    pub hits: Vec<NativeTextHit>,
}

#[derive(Debug, Clone)]
pub enum ContextMenuItem {
    Item {
        label: String,
        icon: Option<ContextMenuIcon>,
        action: ContextMenuAction,
        checked: bool,
        enabled: bool,
        submenu: Vec<ContextMenuItem>,
    },
    Separator,
}

/// Platform-neutral menu icon intent. Native macOS menus translate this to a
/// version-safe SF Symbol and fall back to the same bundled Lucide asset used
/// by ThinkTerm's Windows/Linux menu renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContextMenuIcon {
    Application,
    Back,
    Check,
    Close,
    Code,
    Collapse,
    Copy,
    Cut,
    Delete,
    Edit,
    Expand,
    ExternalLink,
    File,
    Folder,
    FolderAdd,
    FolderRemove,
    Home,
    Info,
    MoveLeft,
    MoveRight,
    New,
    Note,
    Notification,
    Paste,
    Pin,
    Refresh,
    Redo,
    Save,
    Search,
    Server,
    Settings,
    Sidebar,
    Spellcheck,
    SplitHorizontal,
    SplitVertical,
    Stack,
    Terminal,
    Undo,
    Unpin,
    Vault,
    Warning,
    Window,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderPickerOptions {
    pub title: String,
    pub prompt: String,
}

impl Default for FolderPickerOptions {
    fn default() -> Self {
        Self {
            title: "Open Project".to_string(),
            prompt: "Open".to_string(),
        }
    }
}

impl ContextMenuIcon {
    pub fn sf_symbol_name(self) -> &'static str {
        match self {
            Self::Application => "app",
            Self::Back => "chevron.left",
            Self::Check => "checkmark.circle",
            Self::Close => "xmark",
            Self::Code => "chevron.left.forwardslash.chevron.right",
            Self::Collapse => "rectangle.compress.vertical",
            Self::Copy => "doc.on.doc",
            Self::Cut => "scissors",
            Self::Delete => "trash",
            Self::Edit => "pencil",
            Self::Expand => "rectangle.expand.vertical",
            Self::ExternalLink => "arrow.up.right.square",
            Self::File => "doc",
            Self::Folder | Self::Vault => "folder",
            Self::FolderAdd => "folder.badge.plus",
            Self::FolderRemove => "folder.badge.minus",
            Self::Home => "house",
            Self::Info => "info.circle",
            Self::MoveLeft => "arrow.left",
            Self::MoveRight => "arrow.right",
            Self::New => "plus",
            Self::Note => "square.and.pencil",
            Self::Notification => "bell",
            Self::Paste => "doc.on.clipboard",
            Self::Pin => "pin",
            Self::Refresh => "arrow.clockwise",
            Self::Redo => "arrow.uturn.forward",
            Self::Save => "square.and.arrow.down",
            Self::Search => "magnifyingglass",
            Self::Server => "server.rack",
            Self::Settings => "gearshape",
            Self::Sidebar => "sidebar.leading",
            Self::Spellcheck => "textformat.abc.dottedunderline",
            Self::SplitHorizontal => "rectangle.split.2x1",
            Self::SplitVertical => "rectangle.split.1x2",
            Self::Stack => "square.stack",
            Self::Terminal => "terminal",
            Self::Undo => "arrow.uturn.backward",
            Self::Unpin => "pin.slash",
            Self::Warning => "exclamationmark.circle",
            Self::Window => "macwindow",
        }
    }

    pub fn lucide_svg(self) -> &'static [u8] {
        match self {
            Self::Application => include_bytes!("../../third_party/lucide/icons/app-window.svg"),
            Self::Back | Self::MoveLeft => {
                include_bytes!("../../third_party/lucide/icons/arrow-left.svg")
            }
            Self::Check => include_bytes!("../../third_party/lucide/icons/circle-check.svg"),
            Self::Close => include_bytes!("../../third_party/lucide/icons/x.svg"),
            Self::Code => include_bytes!("../../third_party/lucide/icons/code-xml.svg"),
            Self::Collapse => include_bytes!("../../third_party/lucide/icons/shrink.svg"),
            Self::Copy => include_bytes!("../../third_party/lucide/icons/copy.svg"),
            Self::Cut => include_bytes!("../../third_party/lucide/icons/scissors.svg"),
            Self::Delete => include_bytes!("../../third_party/lucide/icons/trash-2.svg"),
            Self::Edit => include_bytes!("../../third_party/lucide/icons/pencil.svg"),
            Self::Expand => include_bytes!("../../third_party/lucide/icons/expand.svg"),
            Self::ExternalLink => {
                include_bytes!("../../third_party/lucide/icons/external-link.svg")
            }
            Self::File => include_bytes!("../../third_party/lucide/icons/file.svg"),
            Self::Folder => include_bytes!("../../third_party/lucide/icons/folder.svg"),
            Self::FolderAdd => include_bytes!("../../third_party/lucide/icons/folder-plus.svg"),
            Self::FolderRemove => {
                include_bytes!("../../third_party/lucide/icons/folder-minus.svg")
            }
            Self::Home => include_bytes!("../../third_party/lucide/icons/house.svg"),
            Self::Info => include_bytes!("../../third_party/lucide/icons/info.svg"),
            Self::MoveRight => include_bytes!("../../third_party/lucide/icons/arrow-right.svg"),
            Self::New => include_bytes!("../../third_party/lucide/icons/plus.svg"),
            Self::Note => include_bytes!("../../third_party/lucide/icons/notebook-tabs.svg"),
            Self::Notification => include_bytes!("../../third_party/lucide/icons/bell.svg"),
            Self::Paste => include_bytes!("../../third_party/lucide/icons/clipboard-paste.svg"),
            Self::Pin => include_bytes!("../../third_party/lucide/icons/pin.svg"),
            Self::Refresh => include_bytes!("../../third_party/lucide/icons/rotate-ccw.svg"),
            Self::Redo => include_bytes!("../../third_party/lucide/icons/redo.svg"),
            Self::Save => include_bytes!("../../third_party/lucide/icons/save.svg"),
            Self::Search => include_bytes!("../../third_party/lucide/icons/search.svg"),
            Self::Server => include_bytes!("../../third_party/lucide/icons/server.svg"),
            Self::Settings => include_bytes!("../../third_party/lucide/icons/settings.svg"),
            Self::Sidebar => include_bytes!("../../third_party/lucide/icons/panel-left.svg"),
            Self::Spellcheck => include_bytes!("../../third_party/lucide/icons/spell-check.svg"),
            Self::SplitHorizontal => {
                include_bytes!("../../third_party/lucide/icons/square-split-horizontal.svg")
            }
            Self::SplitVertical => {
                include_bytes!("../../third_party/lucide/icons/square-split-vertical.svg")
            }
            Self::Stack => include_bytes!("../../third_party/lucide/icons/square-stack.svg"),
            Self::Terminal => include_bytes!("../../third_party/lucide/icons/terminal.svg"),
            Self::Undo => include_bytes!("../../third_party/lucide/icons/undo.svg"),
            Self::Unpin => include_bytes!("../../third_party/lucide/icons/pin-off.svg"),
            Self::Vault => include_bytes!("../../third_party/lucide/icons/folder-tree.svg"),
            Self::Warning => include_bytes!("../../third_party/lucide/icons/circle-alert.svg"),
            Self::Window => include_bytes!("../../third_party/lucide/icons/panels-top-left.svg"),
        }
    }
}

impl ContextMenuItem {
    pub fn item(label: impl Into<String>, action: config::keyassignment::KeyAssignment) -> Self {
        Self::Item {
            label: label.into(),
            icon: None,
            action: ContextMenuAction::KeyAssignment(action),
            checked: false,
            enabled: true,
            submenu: vec![],
        }
    }

    pub fn item_with_icon(
        label: impl Into<String>,
        icon: ContextMenuIcon,
        action: config::keyassignment::KeyAssignment,
    ) -> Self {
        Self::Item {
            label: label.into(),
            icon: Some(icon),
            action: ContextMenuAction::KeyAssignment(action),
            checked: false,
            enabled: true,
            submenu: vec![],
        }
    }

    pub fn application_item(label: impl Into<String>, action_id: u64) -> Self {
        Self::Item {
            label: label.into(),
            icon: None,
            action: ContextMenuAction::ApplicationAction(action_id),
            checked: false,
            enabled: true,
            submenu: vec![],
        }
    }

    pub fn disabled(mut self) -> Self {
        if let Self::Item { enabled, .. } = &mut self {
            *enabled = false;
        }
        self
    }

    pub fn with_icon(mut self, icon_value: ContextMenuIcon) -> Self {
        if let Self::Item { icon, .. } = &mut self {
            *icon = Some(icon_value);
        }
        self
    }

    pub fn checked(mut self, checked_value: bool) -> Self {
        if let Self::Item { checked, .. } = &mut self {
            *checked = checked_value;
        }
        self
    }

    pub fn submenu(label: impl Into<String>, items: Vec<ContextMenuItem>) -> Self {
        Self::Item {
            label: label.into(),
            icon: None,
            action: ContextMenuAction::KeyAssignment(config::keyassignment::KeyAssignment::Nop),
            checked: false,
            enabled: true,
            submenu: items,
        }
    }

    pub fn submenu_with_icon(
        label: impl Into<String>,
        icon: ContextMenuIcon,
        items: Vec<ContextMenuItem>,
    ) -> Self {
        Self::Item {
            label: label.into(),
            icon: Some(icon),
            action: ContextMenuAction::KeyAssignment(config::keyassignment::KeyAssignment::Nop),
            checked: false,
            enabled: true,
            submenu: items,
        }
    }
}

pub type ULength = euclid::Length<usize, PixelUnit>;
pub type Rect = euclid::Rect<isize, PixelUnit>;
pub type RectF = euclid::Rect<f32, PixelUnit>;
pub type Size = euclid::Size2D<isize, PixelUnit>;
pub type SizeF = euclid::Size2D<f32, PixelUnit>;
pub type ScreenRect = euclid::Rect<isize, ScreenPixelUnit>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseCursor {
    Arrow,
    Hand,
    Text,
    SizeUpDown,
    SizeLeftRight,
    SizeNorthWestSouthEast,
    SizeNorthEastSouthWest,
}

/// Represents the preferred appearance of the windowing
/// environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Appearance {
    /// Standard dark-text-on-light-background presentation
    Light,
    /// Dark mode, with predominantly dark or muted colors
    Dark,
    /// dark-text-on-light-background, but in a higher contrast
    /// more accesible palette
    LightHighContrast,
    /// darker background but with higher contrast than regular
    /// dark mode
    DarkHighContrast,
}

impl std::string::ToString for Appearance {
    fn to_string(&self) -> String {
        match self {
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::LightHighContrast => "LightHighContrast",
            Self::DarkHighContrast => "DarkHighContrast",
        }
        .to_string()
    }
}

bitflags! {
    #[derive(Default)]
    pub struct WindowState: u16 {
        /// Occupies the whole screen; cannot be resized while in this state.
        const FULL_SCREEN = 1<<1;
        /// Maximized along either or both of horizontal or vertical dimensions;
        /// cannot be resized while in this state.
        const MAXIMIZED = 1<<2;
        /// Minimized or in some kind of off-screen state. Cannot be repainted
        /// while in this state.
        const HIDDEN = 1<<3;
        /// Always on top (floating) window
        const ALWAYS_ON_TOP = 1<<4;
        /// Always on bottom (docked) window
        const ALWAYS_ON_BOTTOM = 1<<5;
        /// The compositor/window manager is drawing the title bar.
        const SERVER_DECORATED = 1<<6;
        /// The compositor has tiled the window against one or more edges.
        const TILED = 1<<7;
        /// The window system can preserve transparent pixels in the surface.
        const COMPOSITED = 1<<8;
    }
}

impl WindowState {
    pub fn can_resize(self) -> bool {
        !self.intersects(Self::FULL_SCREEN | Self::MAXIMIZED)
    }

    pub fn can_paint(self) -> bool {
        !self.contains(Self::HIDDEN)
    }

    pub fn as_window_level(self) -> WindowLevel {
        if self.contains(Self::ALWAYS_ON_TOP) {
            WindowLevel::AlwaysOnTop
        } else if self.contains(Self::ALWAYS_ON_BOTTOM) {
            WindowLevel::AlwaysOnBottom
        } else {
            WindowLevel::Normal
        }
    }
}

#[derive(Debug, Clone)]
pub enum WindowKeyEvent {
    RawKeyEvent(RawKeyEvent),
    KeyEvent(KeyEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadKeyStatus {
    /// Not in a dead key processing hold
    None,
    /// Holding until composition is done; the string is the uncommitted
    /// composition text to show as a placeholder
    Composing(String),
}

#[derive(Debug)]
pub enum WindowEvent {
    /// Called when the window close button is clicked.
    /// The window closure is deferred and this event is
    /// sent to your application to decide whether it will
    /// really close the window.
    CloseRequested,

    /// Called when the window is being destroyed by the window system
    Destroyed,

    /// Called when the window has been resized
    Resized {
        dimensions: Dimensions,
        window_state: WindowState,
        live_resizing: bool,
    },

    /// Called when a program-requested set_inner_size() has finished
    SetInnerSizeCompleted,

    /// Called when the window has been invalidated and needs to
    /// be repainted
    NeedRepaint,

    /// Called when the window gains/loses focus
    FocusChanged(bool),

    AdviseDeadKeyStatus(DeadKeyStatus),

    NativeTextInputReplace {
        token: u64,
        revision: u64,
        source_range: Range<usize>,
        text: String,
    },

    /// Called to handle a raw key event, prior to any dead key,
    /// keymap composition or other higher level treatment.
    /// If you handle this key event, you must call
    /// event.set_handled() to prevent additional processing.
    RawKeyEvent(RawKeyEvent),

    /// Called to handle a key event.
    KeyEvent(KeyEvent),

    MouseEvent(MouseEvent),
    MouseLeave,

    AppearanceChanged(Appearance),

    ToggleWorkspaceSidebar,

    Notification(Box<dyn Any + Send + Sync>),

    // Called while files are being dragged over the window.
    //
    // `coords` is window-relative, in the same space as `MouseEvent::coords`,
    // so a handler can hit-test it against whatever it painted. It is `None`
    // on platforms that do not report a position (Wayland, Windows), and a
    // handler that needs a position must fall back to its no-position
    // behaviour rather than guessing.
    //
    // `paths` may be empty here even when the drag does carry files: X11's
    // XDND only transfers the payload on drop, so the hover events know a
    // drag is happening without yet knowing what it holds.
    DraggedFile {
        paths: Vec<PathBuf>,
        coords: Option<Point>,
    },

    // Called when a file drag leaves the window or is cancelled, so any
    // drop-target affordance can be taken down.
    DragLeave,

    // Called when the files are dropped into the window
    DroppedFile {
        paths: Vec<PathBuf>,
        coords: Option<Point>,
    },

    // Called when urls are dropped into the window
    DroppedUrl(Vec<Url>),

    // Called when text is dropped into the window
    DroppedString(String),

    /// Called by menubar dispatching stuff on some systems
    PerformKeyAssignment(config::keyassignment::KeyAssignment),

    /// Dispatches an application-private action selected from a native menu.
    PerformContextMenuAction(u64),

    AdviseModifiersLedStatus(Modifiers, KeyboardLedStatus),
}

pub struct WindowEventSender {
    handler: Box<dyn FnMut(WindowEvent, &Window)>,
    window: Option<Window>,
}

impl WindowEventSender {
    pub fn new<F: 'static + FnMut(WindowEvent, &Window)>(handler: F) -> Self {
        Self {
            handler: Box::new(handler),
            window: None,
        }
    }

    pub(crate) fn assign_window(&mut self, window: Window) {
        self.window.replace(window);
    }

    pub fn dispatch(&mut self, event: WindowEvent) {
        if let Some(window) = self.window.as_ref() {
            log::trace!("{:?}", event);
            (self.handler)(event, window);
        }
    }
}

#[derive(Debug, Error)]
#[error("Graphics drivers lost context")]
pub struct GraphicsDriversLostContext {}

#[async_trait(?Send)]
pub trait WindowOps {
    /// Show a hidden window
    fn show(&self);

    fn notify<T: Any + Send + Sync>(&self, t: T)
    where
        Self: Sized;

    /// Setup opengl for rendering
    async fn enable_opengl(&self) -> anyhow::Result<Rc<glium::backend::Context>>;
    /// Advise the window that a frame is finished
    fn finish_frame(&self, frame: glium::Frame) -> anyhow::Result<()> {
        frame.finish()?;
        Ok(())
    }

    /// Hide a visible window
    fn hide(&self);

    /// Schedule the window to be closed
    fn close(&self);

    /// Change the cursor
    fn set_cursor(&self, cursor: Option<MouseCursor>);

    /// Show a native context menu at the specified client-area pixel coordinate.
    fn show_context_menu(&self, _coords: Point, _items: Vec<ContextMenuItem>) {}

    /// Show a native folder picker and invoke the callback with the selected directory.
    fn pick_folder_async(&self, callback: Box<dyn FnOnce(Option<PathBuf>) + 'static>) {
        callback(None);
    }

    fn pick_folder_async_with_options(
        &self,
        _options: FolderPickerOptions,
        callback: Box<dyn FnOnce(Option<PathBuf>) + 'static>,
    ) {
        self.pick_folder_async(callback);
    }

    /// Show a native picker for choosing an application (macOS: .app bundle,
    /// Windows: .exe, Linux: .desktop entry or executable).
    fn pick_app_async(&self, callback: Box<dyn FnOnce(Option<PathBuf>) + 'static>) {
        callback(None);
    }

    /// Invalidate the window so that the entire client area will
    /// be repainted shortly
    fn invalidate(&self);

    /// Change the titlebar text for the window
    fn set_title(&self, title: &str);

    /// Resize the inner or client area of the window
    fn set_inner_size(&self, width: usize, height: usize);

    /// Use for windows snap layouts
    fn set_maximize_button_position(&self, _rect: ScreenRect) {}

    /// Requests the windowing system to start a window drag.
    ///
    /// This is only implemented on backends that handle
    /// window movement on the server side (Wayland).
    fn request_drag_move(&self) {}

    /// Signal to the windowing system that the mouse is over
    /// a window dragging area.
    ///
    /// This is only implemented on backends that need to
    /// know if the mouse is in a drag area to handle the
    /// click before forwarding the event (Windows).
    fn set_window_drag_position(&self, _coords: ScreenPoint) {}

    /// Changes the location of the window on the screen.
    /// The coordinates are of the top left pixel of the
    /// client area.
    ///
    /// This is only implemented on backends that allow
    /// windows to move themselves (not Wayland).
    fn set_window_position(&self, _coords: ScreenPoint) {}

    /// inform the windowing system of the current textual
    /// cursor input location.  This is used primarily for
    /// the platform specific input method editor
    fn set_text_cursor_position(&self, _cursor: Rect) {}

    /// Exposes bounded custom-editor text to native text input services.
    fn set_native_text_input_snapshot(&self, _snapshot: Option<NativeTextInputSnapshot>) {}

    /// Show the platform's dictionary/definition UI for a piece of text.
    fn show_text_definition(&self, _text: &str, _anchor: Rect) {}

    fn text_check_capabilities(&self) -> TextCheckCapabilities {
        TextCheckCapabilities::default()
    }

    fn request_text_check(&self, request: TextCheckRequest) -> Future<TextCheckResponse> {
        Future::ok(TextCheckResponse {
            request_id: request.request_id,
            issues: vec![],
        })
    }

    fn ignore_spelling_word(&self, _document_id: &str, _word: &str) {}

    fn learn_spelling_word(&self, _word: &str) {}

    /// Initiate textual transfer from the clipboard
    fn get_clipboard(&self, clipboard: Clipboard) -> Future<String>;

    /// Set some text in the clipboard
    fn set_clipboard(&self, clipboard: Clipboard, text: String);

    /// Set window level. Depending on the environment and user preferences
    fn set_window_level(&self, _level: WindowLevel) {}

    /// Set the icon for the window.
    /// Depending on the system this may be shown in its titlebar
    /// and/or in the task manager/task switcher
    fn set_icon(&self, _image: Image) {}

    /// Shows or hides ThinkTerm's terminal workspace sidebar button.
    fn set_titlebar_sidebar_button_visible(&self, _visible: bool) {}

    fn maximize(&self) {}
    fn restore(&self) {}
    fn focus(&self) {}

    fn toggle_fullscreen(&self) {}

    fn config_did_change(&self, _config: &config::ConfigHandle) {}

    /// Configure the Window so that the desktop environment
    /// will constrain resizes so that they are multiples of
    /// the x and y values specified.
    /// This may not be supported or respected by the desktop
    /// environment.
    fn set_resize_increments(&self, _incr: ResizeIncrement) {}

    fn get_os_parameters(
        &self,
        _config: &ConfigHandle,
        _window_state: WindowState,
    ) -> anyhow::Result<Option<os::parameters::Parameters>> {
        Ok(None)
    }
}

#[derive(Debug, Clone, Default)]
pub struct RequestedWindowGeometry {
    pub width: Dimension,
    pub height: Dimension,
    pub x: Option<Dimension>,
    pub y: Option<Dimension>,
    pub macos_frame_autosave_name: Option<String>,
    /// Specifies basis for evaluating x/y coords.
    /// Also applies to width/height when computing % based dimensions
    pub origin: GeometryOrigin,
}

#[derive(Debug, Clone)]
pub struct ResolvedGeometry {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: usize,
    pub height: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct ResizeIncrement {
    pub x: u16,
    pub y: u16,
    pub base_width: u16,
    pub base_height: u16,
}

impl ResizeIncrement {
    /// Use this as a readable shorthand for disabling the feature
    pub fn disabled() -> Self {
        Self {
            x: 1,
            y: 1,
            base_width: 0,
            base_height: 0,
        }
    }
}
