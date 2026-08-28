// let () = msg_send! is a common pattern for objc
#![allow(clippy::let_unit_value)]

use super::keycodes::*;
use super::{nsstring, nsstring_to_str, BitmapRef};
use crate::bitmaps::BitmapImage;
use crate::clipboard::Clipboard as ClipboardContext;
use crate::connection::ConnectionOps;
use crate::os::macos::menu::{Menu, MenuItem, RepresentedItem};
use crate::parameters::{Border, Parameters, TitleBar};
use crate::{
    Clipboard, ClipboardContents, Connection, ContextMenuItem, DeadKeyStatus, Dimensions,
    FolderPickerOptions, Handled, Image, KeyCode, KeyEvent, Modifiers, MouseButtons, MouseCursor,
    MouseEvent, MouseEventKind, MousePress, NativeTextInputSnapshot, Point, PreciseScrollDelta,
    RawKeyEvent, Rect, RequestedWindowGeometry, ResizeIncrement, ResolvedGeometry, ScreenPoint,
    ScrollPhase, Size, TextCheckCapabilities, TextCheckIssue, TextCheckRequest, TextCheckResponse,
    ULength, WindowDecorations, WindowEvent, WindowEventSender, WindowOps, WindowState,
};
use anyhow::{anyhow, bail, ensure};
use async_trait::async_trait;
use block2::RcBlock;
use cocoa::appkit::{
    self, CGFloat, NSApplication, NSApplicationActivateIgnoringOtherApps,
    NSApplicationPresentationOptions, NSBackingStoreBuffered, NSEvent, NSEventModifierFlags,
    NSEventPhase, NSImage, NSImageNameApplicationIcon, NSOpenGLContext, NSOpenGLPixelFormat,
    NSPasteboard, NSRunningApplication, NSScreen, NSView, NSViewHeightSizable, NSViewWidthSizable,
    NSWindow, NSWindowStyleMask,
};
use cocoa::base::*;
use cocoa::foundation::{
    NSArray, NSAutoreleasePool, NSFastEnumeration, NSInteger, NSNotFound, NSPoint, NSRect, NSSize,
    NSString, NSUInteger,
};
use config::window::WindowLevel;
use config::{ConfigHandle, RgbaColor, SrgbaTuple};
use core_foundation::base::{CFTypeID, TCFType};
use core_foundation::bundle::{CFBundleGetBundleWithIdentifier, CFBundleGetFunctionPointerForName};
use core_foundation::data::{CFData, CFDataGetBytePtr, CFDataRef};
use core_foundation::string::{CFString, CFStringRef, UniChar};
use core_foundation::{declare_TCFType, impl_TCFType};
use foreign_types::ForeignType;
use objc::declare::ClassDecl;
use objc::rc::{StrongPtr, WeakPtr};
use objc::runtime::{Class, Object, Protocol, Sel};
use objc::*;
use promise::{Future, Promise};
use raw_window_handle::{
    AppKitDisplayHandle, AppKitWindowHandle, DisplayHandle, HandleError, HasDisplayHandle,
    HasWindowHandle, RawDisplayHandle, RawWindowHandle, WindowHandle,
};
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::{c_void, CStr};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use wezterm_font::FontConfiguration;
use wezterm_input_types::{is_ascii_control, IntegratedTitleButtonStyle, KeyboardLedStatus};

#[allow(non_upper_case_globals)]
const NSViewLayerContentsPlacementTopLeft: NSInteger = 11;
#[allow(non_upper_case_globals)]
const NSViewLayerContentsRedrawDuringViewResize: NSInteger = 2;
const THINKTERM_TITLEBAR_SIDEBAR_BUTTON_FALLBACK_X: f64 = 96.0;
const THINKTERM_TITLEBAR_SIDEBAR_BUTTON_GAP: f64 = 8.0;
const THINKTERM_TITLEBAR_SIDEBAR_BUTTON_SIZE: f64 = 30.0;
const THINKTERM_TITLEBAR_SIDEBAR_BUTTON_TAG: NSInteger = 0x7474_7362;
/// userInfo marker on the titlebar sidebar button's tracking area, so the
/// WindowView's mouseEntered:/mouseExited: can tell it apart from the
/// content view's own tracking area.
const THINKTERM_TITLEBAR_SIDEBAR_BUTTON_TRACKING_KEY: &str = "thinkterm_sidebar_button";

/// `NSDragOperation` values. The dragging entered/updated methods return this
/// mask, not a BOOL: the runtime reads a full word, so returning a byte-wide
/// BOOL leaves the rest of the value undefined.
const NS_DRAG_OPERATION_NONE: NSUInteger = 0;
const NS_DRAG_OPERATION_COPY: NSUInteger = 1;

static THINKTERM_PERF_ENABLED: OnceLock<bool> = OnceLock::new();

type PendingWindowNotification = Box<dyn Any + Send + Sync>;

struct PendingNotificationQueue<T> {
    pending: VecDeque<T>,
    draining: bool,
    drain_scheduled: bool,
}

impl<T> Default for PendingNotificationQueue<T> {
    fn default() -> Self {
        Self {
            pending: VecDeque::new(),
            draining: false,
            drain_scheduled: false,
        }
    }
}

impl<T> PendingNotificationQueue<T> {
    fn enqueue(&mut self, item: T) {
        self.pending.push_back(item);
    }

    fn begin_scheduled_drain(&mut self) {
        self.drain_scheduled = false;
    }
}

/// Drain without holding the queue borrow across `dispatch`. That is what
/// lets a notification handler enqueue another notification without either
/// re-entering the window state or overtaking work that was already queued.
/// Returns true when the caller must schedule one future drain attempt.
fn drain_pending_notifications<T>(
    queue: &RefCell<PendingNotificationQueue<T>>,
    mut dispatch: impl FnMut(T) -> Result<(), T>,
) -> bool {
    {
        let mut queue = queue.borrow_mut();
        if queue.draining {
            return false;
        }
        queue.draining = true;
    }

    let mut schedule_retry = false;
    loop {
        let Some(item) = queue.borrow_mut().pending.pop_front() else {
            break;
        };
        if let Err(item) = dispatch(item) {
            let mut queue = queue.borrow_mut();
            queue.pending.push_front(item);
            if !queue.drain_scheduled {
                queue.drain_scheduled = true;
                schedule_retry = true;
            }
            break;
        }
    }
    queue.borrow_mut().draining = false;
    schedule_retry
}

#[cfg(test)]
mod pending_notification_tests {
    use super::{drain_pending_notifications, PendingNotificationQueue};
    use std::cell::RefCell;

    #[test]
    fn busy_notifications_retry_once_and_keep_fifo_order() {
        let queue = RefCell::new(PendingNotificationQueue::default());
        queue.borrow_mut().enqueue(1);
        queue.borrow_mut().enqueue(2);

        assert!(drain_pending_notifications(&queue, Err));
        assert_eq!(queue.borrow().pending.iter().copied().collect::<Vec<_>>(), [1, 2]);
        assert!(queue.borrow().drain_scheduled);

        queue.borrow_mut().enqueue(3);
        assert!(
            !drain_pending_notifications(&queue, Err),
            "an already scheduled retry must not be duplicated"
        );

        queue.borrow_mut().begin_scheduled_drain();
        let mut delivered = Vec::new();
        assert!(!drain_pending_notifications(&queue, |item| {
            delivered.push(item);
            Ok(())
        }));
        assert_eq!(delivered, [1, 2, 3]);
        assert!(queue.borrow().pending.is_empty());
    }

    #[test]
    fn reentrant_notifications_append_after_the_existing_backlog() {
        let queue = RefCell::new(PendingNotificationQueue::default());
        queue.borrow_mut().enqueue(1);
        queue.borrow_mut().enqueue(2);

        let mut delivered = Vec::new();
        assert!(!drain_pending_notifications(&queue, |item| {
            delivered.push(item);
            if item == 1 {
                queue.borrow_mut().enqueue(3);
            }
            Ok(())
        }));
        assert_eq!(delivered, [1, 2, 3]);
    }
}

fn thinkterm_perf_enabled() -> bool {
    *THINKTERM_PERF_ENABLED.get_or_init(|| {
        std::env::var_os("THINKTERM_PERF")
            .map(|value| value != "0" && !value.is_empty())
            .unwrap_or(false)
    })
}

fn spell_document_tag(document_id: &str) -> NSInteger {
    let mut hasher = DefaultHasher::new();
    document_id.hash(&mut hasher);
    (hasher.finish() & i64::MAX as u64).max(1) as NSInteger
}

fn byte_offset_for_utf16(text: &str, target: usize) -> usize {
    if target == 0 {
        return 0;
    }
    let mut utf16 = 0usize;
    for (byte, ch) in text.char_indices() {
        let next = utf16 + ch.len_utf16();
        if next > target {
            return byte;
        }
        utf16 = next;
        if utf16 == target {
            return byte + ch.len_utf8();
        }
    }
    text.len()
}

fn utf16_offset_for_byte(text: &str, byte: usize) -> usize {
    let byte = byte.min(text.len());
    let mut boundary = byte;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text[..boundary].encode_utf16().count()
}

#[cfg(test)]
mod native_text_range_tests {
    use super::{byte_offset_for_utf16, utf16_offset_for_byte};

    #[test]
    fn utf8_and_utf16_offsets_round_trip_at_character_boundaries() {
        let text = "a😀你e\u{301}";
        for (byte, _) in text
            .char_indices()
            .chain(std::iter::once((text.len(), '\0')))
        {
            let utf16 = utf16_offset_for_byte(text, byte);
            assert_eq!(byte_offset_for_utf16(text, utf16), byte);
        }
    }
}

fn macos_current_screen_max_fps() -> Option<usize> {
    unsafe {
        let screen = NSScreen::mainScreen(nil);
        if screen.is_null() {
            return None;
        }
        let has_max_fps: BOOL = msg_send!(screen, respondsToSelector: sel!(maximumFramesPerSecond));
        if has_max_fps == YES {
            let max_fps: NSInteger = msg_send!(screen, maximumFramesPerSecond);
            Some(max_fps.max(1) as usize)
        } else {
            None
        }
    }
}

fn target_frame_fps(configured_max_fps: u64) -> f64 {
    let configured = configured_max_fps.max(1) as f64;
    let screen = macos_current_screen_max_fps().unwrap_or(60).max(1) as f64;
    configured.min(screen).max(1.0)
}

/// How long to hold the next frame back, or `None` to let the display decide.
///
/// The throttle exists to honour a `max_fps` set *below* the panel's rate. When
/// it is not below it -- and the default 120 is not below a 120Hz panel -- there
/// is nothing left to enforce, because CoreAnimation already delivers `drawRect`
/// once per refresh. Running the timer anyway is not merely redundant, it costs
/// a refresh: the delay was measured from the *end* of the paint, so a 7ms frame
/// was followed by a full 8.3ms of enforced idleness and only then by the wait
/// for the next vsync, and every `drawRect` AppKit delivered in between was
/// dropped by the throttle check. Measured: delivered frames every 33ms on a
/// 120Hz panel while painting took 7ms.
fn frame_throttle_delay(
    configured_max_fps: u64,
    spent: std::time::Duration,
) -> Option<std::time::Duration> {
    let screen = macos_current_screen_max_fps().unwrap_or(60).max(1) as f64;
    if configured_max_fps.max(1) as f64 >= screen {
        return None;
    }
    let period = std::time::Duration::from_secs_f64(1.0 / target_frame_fps(configured_max_fps));
    let remaining = period.saturating_sub(spent);
    (!remaining.is_zero()).then_some(remaining)
}

fn ns_event_phase_to_scroll_phase(phase: NSEventPhase) -> Option<ScrollPhase> {
    if phase.contains(NSEventPhase::NSEventPhaseBegan) {
        Some(ScrollPhase::Began)
    } else if phase.contains(NSEventPhase::NSEventPhaseStationary) {
        Some(ScrollPhase::Stationary)
    } else if phase.contains(NSEventPhase::NSEventPhaseChanged) {
        Some(ScrollPhase::Changed)
    } else if phase.contains(NSEventPhase::NSEventPhaseEnded) {
        Some(ScrollPhase::Ended)
    } else if phase.contains(NSEventPhase::NSEventPhaseCancelled) {
        Some(ScrollPhase::Cancelled)
    } else if phase.contains(NSEventPhase::NSEventPhaseMayBegin) {
        Some(ScrollPhase::MayBegin)
    } else {
        None
    }
}

fn should_dispatch_scroll_event(
    vert_delta: f64,
    horz_delta: f64,
    has_precise_delta: bool,
    scroll_phase: Option<ScrollPhase>,
    momentum_phase: Option<ScrollPhase>,
) -> bool {
    let phase_only_completion = matches!(
        scroll_phase,
        Some(ScrollPhase::Ended | ScrollPhase::Cancelled)
    ) || matches!(
        momentum_phase,
        Some(ScrollPhase::Ended | ScrollPhase::Cancelled)
    );
    vert_delta.abs() >= 1.0 || horz_delta.abs() >= 1.0 || has_precise_delta || phase_only_completion
}

#[cfg(test)]
mod scroll_phase_tests {
    use super::{should_dispatch_scroll_event, ScrollPhase};

    #[test]
    fn zero_delta_scroll_completion_is_still_dispatched() {
        assert!(should_dispatch_scroll_event(
            0.0,
            0.0,
            false,
            Some(ScrollPhase::Ended),
            None,
        ));
        assert!(should_dispatch_scroll_event(
            0.0,
            0.0,
            false,
            None,
            Some(ScrollPhase::Cancelled),
        ));
        assert!(!should_dispatch_scroll_event(
            0.0,
            0.0,
            false,
            Some(ScrollPhase::Stationary),
            None,
        ));
    }
}

unsafe fn set_view_background_color(view: id, color: RgbaColor) {
    if view.is_null() {
        return;
    }

    let layer: id = msg_send![view, layer];
    if !layer.is_null() {
        let srgb_cgcolor = objc2_core_graphics::CGColor::new_srgb(
            color.0.into(),
            color.1.into(),
            color.2.into(),
            color.3.into(),
        );
        let _: () = msg_send![layer, setBackgroundColor: srgb_cgcolor];
    }
}

unsafe fn set_standard_window_buttons_visible(window: &StrongPtr, visible: bool) {
    let hidden = if visible { NO } else { YES };
    let alpha = if visible { 1.0 } else { 0.0 };
    let enabled = if visible { YES } else { NO };

    for titlebar_button in &[
        appkit::NSWindowButton::NSWindowMiniaturizeButton,
        appkit::NSWindowButton::NSWindowCloseButton,
        appkit::NSWindowButton::NSWindowZoomButton,
    ] {
        let button = window.standardWindowButton_(*titlebar_button);
        if !button.is_null() {
            let _: () = msg_send![button, setHidden: hidden];
            let _: () = msg_send![button, setAlphaValue: alpha];
            let _: () = msg_send![button, setEnabled: enabled];
        }
    }
}

pub fn set_application_icon_from_file(path: &Path) -> anyhow::Result<()> {
    // Loading the icon decodes several MB of NSImage, and every copy handed to
    // setApplicationIconImage_ stays alive. Every new window applies the icon,
    // so reloading per call leaks one decoded image per window.
    static LAST_ICON_PATH: std::sync::Mutex<Option<std::path::PathBuf>> =
        std::sync::Mutex::new(None);
    if LAST_ICON_PATH.lock().unwrap().as_deref() == Some(path) {
        return Ok(());
    }

    let path_string = path.to_string_lossy();

    unsafe {
        let ns_image = NSImage::alloc(nil).initWithContentsOfFile_(*nsstring(path_string.as_ref()));
        if ns_image == nil {
            bail!("failed to load application icon from {}", path.display());
        }

        let ns_image = StrongPtr::new(ns_image);
        let _: BOOL = msg_send![*ns_image, setName:NSImageNameApplicationIcon];
        let app = NSApplication::sharedApplication(nil);
        app.setApplicationIconImage_(*ns_image);
    }

    *LAST_ICON_PATH.lock().unwrap() = Some(path.to_path_buf());
    Ok(())
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGSMainConnectionID() -> id;
    fn CGSSetWindowBackgroundBlurRadius(
        connection_id: id,
        window_id: NSInteger,
        radius: i64,
    ) -> i32;
}

fn round_away_from_zerof(value: f64) -> f64 {
    if value > 0. {
        value.max(1.).round()
    } else {
        value.min(-1.).round()
    }
}

fn round_away_from_zero(value: f64) -> i16 {
    if value > 0. {
        value.max(1.).round() as i16
    } else {
        value.min(-1.).round() as i16
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ImeDisposition {
    /// Nothing happened
    None,
    /// IME triggered an action
    Acted,
    /// We decided to continue with key dispatch
    Continue,
}

#[repr(C)]
struct NSRange(cocoa::foundation::NSRange);

#[derive(Debug)]
#[repr(C)]
struct NSRangePointer(*mut NSRange);

impl std::fmt::Debug for NSRange {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        fmt.debug_struct("NSRange")
            .field("location", &self.0.location)
            .field("length", &self.0.length)
            .finish()
    }
}

unsafe impl objc::Encode for NSRange {
    fn encode() -> objc::Encoding {
        let encoding = format!(
            "{{NSRange={}{}}}",
            NSUInteger::encode().as_str(),
            NSUInteger::encode().as_str()
        );
        unsafe { objc::Encoding::from_str(&encoding) }
    }
}

unsafe impl objc::Encode for NSRangePointer {
    fn encode() -> objc::Encoding {
        unsafe { objc::Encoding::from_str(&format!("^{}", NSRange::encode().as_str())) }
    }
}

impl NSRange {
    fn new(location: u64, length: u64) -> Self {
        Self(cocoa::foundation::NSRange { location, length })
    }
}

#[derive(Clone)]
pub enum BackendImpl {
    Cgl(Rc<cglbits::GlState>),
    Egl(Rc<crate::egl::GlState>),
}

impl BackendImpl {
    pub fn update(&self) {
        if let Self::Cgl(be) = self {
            be.update();
        }
    }
}

#[derive(Clone)]
pub struct GlContextPair {
    pub context: Rc<glium::backend::Context>,
    pub backend: BackendImpl,
}

impl GlContextPair {
    /// on macOS we first try to initialize EGL by dynamically loading it.
    /// The system doesn't provide an EGL implementation, but the ANGLE
    /// project (and MetalANGLE) both provide implementations.
    /// The ANGLE EGL implementation wants a CALayer descendant passed
    /// as the EGLNativeWindowType.
    pub fn create(view: id) -> anyhow::Result<Self> {
        let behavior = if cfg!(debug_assertions) {
            glium::debug::DebugCallbackBehavior::DebugMessageOnError
        } else {
            glium::debug::DebugCallbackBehavior::Ignore
        };

        // Let's first try to initialize EGL...
        let (context, backend) = match if config::configuration().prefer_egl {
            // ANGLE wants a layer, so tell the view to create one.
            // Importantly, we must set its scale to 1.0 prior to initializing
            // EGL to prevent undesirable scaling.
            let layer: id;
            unsafe {
                let _: () = msg_send![view, setWantsLayer: YES];
                layer = msg_send![view, layer];
                let _: () = msg_send![layer, setContentsScale: 1.0f64];
                let _: () = msg_send![layer, setOpaque: NO];
            };

            let conn = Connection::get().unwrap();

            let state = match conn.gl_connection.borrow().as_ref() {
                None => crate::egl::GlState::create(None, layer as *const c_void),
                Some(glconn) => crate::egl::GlState::create_with_existing_connection(
                    glconn,
                    layer as *const c_void,
                ),
            };

            if state.is_ok() {
                conn.gl_connection
                    .borrow_mut()
                    .replace(Rc::clone(state.as_ref().unwrap().get_connection()));

                // ANGLE will create a CAMetalLayer as a sublayer of our provided
                // layer.  Even though CALayer defaults to !opaque, CAMetalLayer
                // defaults to opaque, so we need to find that layer and fix
                // the opacity so that our alpha values are respected.
                unsafe {
                    let sublayers: id = msg_send![layer, sublayers];
                    let layer_count = sublayers.count();
                    for i in 0..layer_count {
                        let layer = sublayers.objectAtIndex(i);
                        let _: () = msg_send![layer, setOpaque: NO];
                    }
                }
            }

            state
        } else {
            Err(anyhow!("prefers not to use EGL"))
        } {
            Ok(backend) => {
                let backend = Rc::new(backend);
                let context =
                    unsafe { glium::backend::Context::new(Rc::clone(&backend), true, behavior) }?;
                (context, BackendImpl::Egl(backend))
            }
            // ... and then fallback to the deprecated platform provided CGL
            Err(err) => {
                log::debug!("EGL init failed: {:#}, falling back to CGL", err);
                let backend = Rc::new(cglbits::GlState::create(view)?);
                let context =
                    unsafe { glium::backend::Context::new(Rc::clone(&backend), true, behavior) }?;
                (context, BackendImpl::Cgl(backend))
            }
        };

        Ok(Self { context, backend })
    }
}

mod cglbits {
    use super::*;

    pub struct GlState {
        _pixel_format: StrongPtr,
        gl_context: StrongPtr,
    }

    impl GlState {
        pub fn create(view: id) -> anyhow::Result<Self> {
            log::trace!("Calling NSOpenGLPixelFormat::initWithAttributes");
            let pixel_format = unsafe {
                StrongPtr::new(NSOpenGLPixelFormat::alloc(nil).initWithAttributes_(&[
                    appkit::NSOpenGLPFAOpenGLProfile as u32,
                    appkit::NSOpenGLProfileVersion3_2Core as u32,
                    appkit::NSOpenGLPFAClosestPolicy as u32,
                    appkit::NSOpenGLPFAColorSize as u32,
                    32,
                    appkit::NSOpenGLPFAAlphaSize as u32,
                    8,
                    appkit::NSOpenGLPFADepthSize as u32,
                    24,
                    appkit::NSOpenGLPFAStencilSize as u32,
                    8,
                    appkit::NSOpenGLPFAAllowOfflineRenderers as u32,
                    appkit::NSOpenGLPFAAccelerated as u32,
                    appkit::NSOpenGLPFADoubleBuffer as u32,
                    0,
                ]))
            };
            log::trace!("NSOpenGLPixelFormat::initWithAttributes returned");
            ensure!(
                !pixel_format.is_null(),
                "failed to create NSOpenGLPixelFormat"
            );

            // Allow using retina resolutions; without this we're forced into low res
            // and the system will scale us up, resulting in blurry rendering
            unsafe {
                let _: () = msg_send![view, setWantsBestResolutionOpenGLSurface: YES];
            }

            let gl_context = unsafe {
                StrongPtr::new(
                    NSOpenGLContext::alloc(nil).initWithFormat_shareContext_(*pixel_format, nil),
                )
            };
            ensure!(!gl_context.is_null(), "failed to create NSOpenGLContext");

            unsafe {
                let opaque: cgl::GLint = 0;
                gl_context.setValues_forParameter_(
                    &opaque,
                    cocoa::appkit::NSOpenGLContextParameter::NSOpenGLCPSurfaceOpacity,
                );

                gl_context.setView_(view);

                // Explicitly disable vsync; we'll manage throttling frames at
                // the application level
                let swap_interval: cgl::GLint = 0;
                gl_context.setValues_forParameter_(
                    &swap_interval,
                    cocoa::appkit::NSOpenGLContextParameter::NSOpenGLCPSwapInterval,
                );
            }

            Ok(Self {
                _pixel_format: pixel_format,
                gl_context,
            })
        }

        /// Calls NSOpenGLContext update; we need to do this on resize
        pub fn update(&self) {
            unsafe {
                let _: () = msg_send![*self.gl_context, update];
            }
        }
    }

    unsafe impl glium::backend::Backend for GlState {
        fn resize(&self, _: (u32, u32)) {
            todo!()
        }

        fn swap_buffers(&self) -> Result<(), glium::SwapBuffersError> {
            unsafe {
                let pool = NSAutoreleasePool::new(nil);
                self.gl_context.flushBuffer();
                let _: () = msg_send![pool, release];
            }
            Ok(())
        }

        unsafe fn get_proc_address(&self, symbol: &str) -> *const c_void {
            let symbol_name: CFString = FromStr::from_str(symbol).unwrap();
            let framework_name: CFString = FromStr::from_str("com.apple.opengl").unwrap();
            let framework = CFBundleGetBundleWithIdentifier(framework_name.as_concrete_TypeRef());
            let symbol =
                CFBundleGetFunctionPointerForName(framework, symbol_name.as_concrete_TypeRef());
            symbol as *const _
        }

        fn get_framebuffer_dimensions(&self) -> (u32, u32) {
            unsafe {
                let view = self.gl_context.view();
                let frame = NSView::frame(view);
                let backing_frame = NSView::convertRectToBacking(view, frame);
                (
                    backing_frame.size.width as u32,
                    backing_frame.size.height as u32,
                )
            }
        }

        fn is_current(&self) -> bool {
            unsafe {
                let pool = NSAutoreleasePool::new(nil);
                let current = NSOpenGLContext::currentContext(nil);
                let res = if current != nil {
                    let is_equal: BOOL = msg_send![current, isEqual: *self.gl_context];
                    is_equal != NO
                } else {
                    false
                };
                let _: () = msg_send![pool, release];
                res
            }
        }

        unsafe fn make_current(&self) {
            let _: () = msg_send![*self.gl_context, update];
            self.gl_context.makeCurrentContext();
        }
    }
}

pub(crate) struct WindowInner {
    view: StrongPtr,
    window: StrongPtr,
    config: ConfigHandle,
    titlebar_sidebar_button_visible: bool,
}

fn function_key_to_keycode(function_key: char) -> KeyCode {
    // FIXME: CTRL-C is 0x3, should it be normalized to C here
    // using the unmod string?  Or should be normalize the 0x3
    // as the canonical representation of that input?
    match function_key as u16 {
        appkit::NSUpArrowFunctionKey => KeyCode::UpArrow,
        appkit::NSDownArrowFunctionKey => KeyCode::DownArrow,
        appkit::NSLeftArrowFunctionKey => KeyCode::LeftArrow,
        appkit::NSRightArrowFunctionKey => KeyCode::RightArrow,
        appkit::NSHomeFunctionKey => KeyCode::Home,
        appkit::NSEndFunctionKey => KeyCode::End,
        appkit::NSPageUpFunctionKey => KeyCode::PageUp,
        appkit::NSPageDownFunctionKey => KeyCode::PageDown,
        appkit::NSClearLineFunctionKey => KeyCode::NumLock,
        value @ appkit::NSF1FunctionKey..=appkit::NSF35FunctionKey => {
            KeyCode::Function((value - appkit::NSF1FunctionKey + 1) as u8)
        }
        appkit::NSInsertFunctionKey => KeyCode::Insert,
        appkit::NSDeleteFunctionKey => KeyCode::Char('\u{7f}'),
        appkit::NSPrintScreenFunctionKey => KeyCode::PrintScreen,
        appkit::NSScrollLockFunctionKey => KeyCode::ScrollLock,
        appkit::NSPauseFunctionKey => KeyCode::Pause,
        appkit::NSBreakFunctionKey => KeyCode::Cancel,
        appkit::NSPrintFunctionKey => KeyCode::Print,
        _ => KeyCode::Char(function_key),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct Window {
    id: usize,
    ns_window: *mut Object,
    ns_view: *mut Object,
}

unsafe impl Send for Window {}
unsafe impl Sync for Window {}

fn set_window_position(window: *mut Object, coords: ScreenPoint) {
    unsafe {
        let cartesian = screen_point_to_cartesian(coords);
        let frame = NSWindow::frame(window);
        let content_frame = NSWindow::contentRectForFrameRect_(window, frame);
        let delta_x = content_frame.origin.x - frame.origin.x;
        let delta_y = content_frame.origin.y - frame.origin.y;
        let point = NSPoint::new(
            cartesian.x as f64 - delta_x,
            cartesian.y as f64 - delta_y - content_frame.size.height,
        );
        NSWindow::setFrameOrigin_(window, point);
    }
}

impl Window {
    pub async fn new_window<F>(
        _class_name: &str,
        name: &str,
        geometry: RequestedWindowGeometry,
        config: Option<&ConfigHandle>,
        _font_config: Rc<FontConfiguration>,
        event_handler: F,
    ) -> anyhow::Result<Window>
    where
        F: 'static + FnMut(WindowEvent, &Window),
    {
        let config = match config {
            Some(c) => c.clone(),
            None => config::configuration(),
        };

        let conn = Connection::get().expect("new_window called on gui thread");
        let macos_frame_autosave_name = geometry.macos_frame_autosave_name.clone();
        let ResolvedGeometry {
            width,
            height,
            x,
            y,
        } = conn.resolve_geometry(geometry);

        let scale_factor = (conn.default_dpi() / crate::DEFAULT_DPI) as usize;
        let width = width / scale_factor;
        let height = height / scale_factor;
        let x = x.map(|x| x / scale_factor as i32);
        let y = y.map(|y| y / scale_factor as i32);

        let initial_pos = match (x, y) {
            (Some(x), Some(y)) => Some(ScreenPoint::new(x as isize, y as isize)),
            _ => None,
        };

        unsafe {
            let style_mask = decoration_to_mask(
                config.window_decorations,
                config.integrated_title_button_style,
            );
            let rect = NSRect::new(
                NSPoint::new(0., 0.),
                NSSize::new(width as f64, height as f64),
            );

            let conn = Connection::get().expect("Connection::init has not been called");

            let window_id = conn.next_window_id();
            let events = WindowEventSender::new(event_handler);

            let inner = Rc::new(RefCell::new(Inner {
                events,
                view_id: None,
                window_id,
                window: None,
                titlebar_sidebar_button_visible: false,
                screen_changed: false,
                gl_context_pair: None,
                text_cursor_position: Rect::new(Point::new(0, 0), Size::new(0, 0)),
                tracking_rect_tag: 0,
                tracking_rect_size: (0.0, 0.0),
                hscroll_remainder: 0.,
                vscroll_remainder: 0.,
                last_wheel: Instant::now(),
                last_repaint_time: None,
                key_is_down: None,
                dead_pending: None,
                fullscreen: None,
                config: config.clone(),
                ime_state: ImeDisposition::None,
                ime_last_event: None,
                live_resizing: false,
                ime_text: String::new(),
                native_text_input_snapshot: None,
            }));

            let window: id = msg_send![get_window_class(), alloc];
            let window = StrongPtr::new(NSWindow::initWithContentRect_styleMask_backing_defer_(
                window,
                rect,
                style_mask,
                NSBackingStoreBuffered,
                NO,
            ));

            apply_decorations_to_window(
                &window,
                config.window_decorations,
                config.integrated_title_button_style,
            );

            // Prevent Cocoa native tabs from being used
            let _: () = msg_send![*window, setTabbingMode:2 /* NSWindowTabbingModeDisallowed */];
            let restored_from_frame_autosave =
                if let Some(name) = macos_frame_autosave_name.as_deref() {
                    let restored: BOOL = msg_send![*window, setFrameAutosaveName: *nsstring(name)];
                    let _: () = msg_send![*window, setRestorable: YES];
                    restored == YES
                } else {
                    let _: () = msg_send![*window, setRestorable: NO];
                    false
                };

            window.setReleasedWhenClosed_(NO);
            window.setBackgroundColor_(cocoa::appkit::NSColor::clearColor(nil));

            // Tell Cocoa that we output in sRGB, so it handles color space
            // conversion for non-sRGB displays.
            window.setColorSpace_(cocoa::appkit::NSColorSpace::sRGBColorSpace(nil));

            // We could set this, but it makes the entire window, including
            // its titlebar, opaque to this fixed degree.
            // window.setAlphaValue_(0.4);

            // Window positioning: the first window opens up in the center of
            // the screen.  Subsequent windows will be offset from the position
            // of the prior window at the time it was created.  It's not a
            // perfect algorithm by any means, and doesn't take in account
            // windows moving and closing since the last creation, but it is
            // better than creating them all centered which is what we used
            // to do here.
            thread_local! {
                static LAST_POSITION: RefCell<Option<NSPoint>> = RefCell::new(None);
            }

            let frame = NSWindow::frame(*window);
            let active_screen = NSScreen::mainScreen(nil);
            let active_screen_frame = NSScreen::frame(active_screen);

            fn point_in_rect(pt: NSPoint, rect: NSRect) -> bool {
                let rect: euclid::Rect<f64, ()> = euclid::rect(
                    rect.origin.x,
                    rect.origin.y,
                    rect.size.width,
                    rect.size.height,
                );
                rect.contains(euclid::point2(pt.x, pt.y))
            }

            LAST_POSITION.with(|last_pos| {
                if restored_from_frame_autosave {
                    return;
                }
                if let Some(pos) = initial_pos {
                    // Put it where they asked it to be, without influencing
                    // future positioning info
                    set_window_position(*window, pos);
                    return;
                }
                let pos = last_pos.borrow_mut().take();
                let next_pos = match pos {
                    Some(pos) if point_in_rect(pos, active_screen_frame) => {
                        // Only continue the cascade if the prior point is
                        // still within the currently active screen
                        window.cascadeTopLeftFromPoint_(pos)
                    }
                    _ => {
                        // Otherwise, position as if it is the first time
                        // we're displaying on this screen
                        window.center();
                        window.cascadeTopLeftFromPoint_(frame.origin)
                    }
                };
                last_pos.borrow_mut().replace(next_pos);
            });

            window.setTitle_(*nsstring(&name));
            window.setAcceptsMouseMovedEvents_(YES);

            let view = WindowView::init_with_frame(&inner, rect)?;
            view.setAutoresizingMask_(NSViewHeightSizable | NSViewWidthSizable);

            let () = msg_send![
                *view,
                setLayerContentsPlacement: NSViewLayerContentsPlacementTopLeft
            ];

            CGSSetWindowBackgroundBlurRadius(
                CGSMainConnectionID(),
                window.windowNumber(),
                config.macos_window_background_blur,
            );
            window.setContentView_(*view);
            window.setDelegate_(*view);

            view.setWantsLayer(YES);
            let () = msg_send![
                *view,
                setLayerContentsRedrawPolicy: NSViewLayerContentsRedrawDuringViewResize
            ];

            // register for drag and drop operations.
            let () = msg_send![
                *window,
                registerForDraggedTypes:
                    NSArray::arrayWithObject(nil, appkit::NSFilenamesPboardType)
            ];

            let frame = NSView::frame(*view);
            let backing_frame = NSView::convertRectToBacking(*view, frame);
            let width = backing_frame.size.width;
            let height = backing_frame.size.height;

            let dpi = dpi_for_window_screen(*window, &config)
                .unwrap_or(crate::DEFAULT_DPI * (backing_frame.size.width / frame.size.width))
                as usize;

            let weak_window = window.weak();
            let window_handle = Window {
                id: window_id,
                ns_window: *window,
                ns_view: *view,
            };
            let window_inner = Rc::new(RefCell::new(WindowInner {
                window,
                view,
                config: config.clone(),
                titlebar_sidebar_button_visible: false,
            }));
            inner.borrow_mut().window.replace(weak_window);
            conn.windows
                .borrow_mut()
                .insert(window_id, Rc::clone(&window_inner));

            inner
                .borrow_mut()
                .events
                .assign_window(window_handle.clone());

            window_handle.config_did_change(&config);

            // Synthesize a resize event immediately; this allows
            // the embedding application an opportunity to discover
            // the dpi and adjust for display scaling
            inner.borrow_mut().events.dispatch(WindowEvent::Resized {
                dimensions: Dimensions {
                    pixel_width: width as usize,
                    pixel_height: height as usize,
                    dpi,
                },
                window_state: WindowState::default(),
                live_resizing: false,
            });

            Ok(window_handle)
        }
    }
}

impl HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        unsafe {
            Ok(DisplayHandle::borrow_raw(RawDisplayHandle::AppKit(
                AppKitDisplayHandle::new(),
            )))
        }
    }
}

impl HasWindowHandle for Window {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let handle =
            AppKitWindowHandle::new(NonNull::new(self.ns_view as *mut _).expect("non-null"));
        unsafe { Ok(WindowHandle::borrow_raw(RawWindowHandle::AppKit(handle))) }
    }
}

/// @see https://developer.apple.com/documentation/appkit/nswindow/level
pub type NSWindowLevel = i64;

pub fn nswindow_level_to_window_level(nswindow_level: NSWindowLevel) -> WindowLevel {
    match nswindow_level {
        -1 => WindowLevel::AlwaysOnBottom,
        0 => WindowLevel::Normal,
        3 => WindowLevel::AlwaysOnTop,
        _ => panic!("Invalid window level: {}", nswindow_level),
    }
}

pub fn window_level_to_nswindow_level(level: WindowLevel) -> NSWindowLevel {
    match level {
        WindowLevel::AlwaysOnBottom => -1,
        WindowLevel::Normal => 0,
        WindowLevel::AlwaysOnTop => 3,
    }
}

#[async_trait(?Send)]
impl WindowOps for Window {
    async fn enable_opengl(&self) -> anyhow::Result<Rc<glium::backend::Context>> {
        let window_id = self.id;
        promise::spawn::spawn(async move {
            if let Some(handle) = Connection::get().unwrap().window_by_id(window_id) {
                let mut inner = handle.borrow_mut();
                inner.enable_opengl()
            } else {
                bail!("invalid window");
            }
        })
        .await
    }

    fn notify<T: Any + Send + Sync>(&self, t: T)
    where
        Self: Sized,
    {
        Connection::with_window_inner(self.id, move |inner| {
            if let Some(window_view) = WindowView::get_this(unsafe { &**inner.view }) {
                window_view.enqueue_notification(Box::new(t), *inner.view);
            }
            Ok(())
        });
    }

    fn close(&self) {
        Connection::with_window_inner(self.id, |inner| {
            inner.close();
            Ok(())
        });
    }

    fn focus(&self) {
        Connection::with_window_inner(self.id, |inner| {
            inner.focus();
            Ok(())
        });
    }

    fn hide(&self) {
        Connection::with_window_inner(self.id, |inner| {
            inner.hide();
            Ok(())
        });
    }

    fn show(&self) {
        Connection::with_window_inner(self.id, |inner| {
            inner.show();
            Ok(())
        });
    }

    fn set_cursor(&self, cursor: Option<MouseCursor>) {
        Connection::with_window_inner(self.id, move |inner| {
            let _ = inner.set_cursor(cursor);
            Ok(())
        });
    }

    fn show_context_menu(&self, coords: Point, items: Vec<ContextMenuItem>) {
        Connection::with_window_inner(self.id, move |inner| {
            inner.show_context_menu(coords, items);
            Ok(())
        });
    }

    fn pick_folder_async(&self, callback: Box<dyn FnOnce(Option<PathBuf>) + 'static>) {
        self.pick_folder_async_with_options(FolderPickerOptions::default(), callback);
    }

    fn pick_folder_async_with_options(
        &self,
        options: FolderPickerOptions,
        callback: Box<dyn FnOnce(Option<PathBuf>) + 'static>,
    ) {
        unsafe {
            let _pool = NSAutoreleasePool::new(nil);
            let panel: id = msg_send![class!(NSOpenPanel), openPanel];
            let panel = StrongPtr::retain(panel);
            let () = msg_send![*panel, setCanChooseFiles: NO];
            let () = msg_send![*panel, setCanChooseDirectories: YES];
            let () = msg_send![*panel, setAllowsMultipleSelection: NO];
            let () = msg_send![*panel, setCanCreateDirectories: YES];
            let () = msg_send![*panel, setResolvesAliases: YES];
            let title = nsstring(&options.title);
            let prompt = nsstring(&options.prompt);
            let () = msg_send![*panel, setTitle: *title];
            let () = msg_send![*panel, setPrompt: *prompt];
            // Bound to a local so the NSString outlives the NSURL that borrows
            // it; `setDirectoryURL` is only a hint, so a path AppKit dislikes
            // simply leaves the panel wherever it would have opened.
            if let Some(directory) = options.directory.as_ref().and_then(|dir| dir.to_str()) {
                let directory = nsstring(directory);
                let url: id = msg_send![class!(NSURL), fileURLWithPath: *directory isDirectory: YES];
                if url != nil {
                    let () = msg_send![*panel, setDirectoryURL: url];
                }
            }

            const NS_MODAL_RESPONSE_OK: NSInteger = 1;
            let callback = Arc::new(Mutex::new(Some(callback)));
            let callback_for_block = callback.clone();
            let panel_for_block = panel.clone();
            let block = RcBlock::new(move |result: NSInteger| {
                let selected_path = if result != NS_MODAL_RESPONSE_OK {
                    None
                } else {
                    let url: id = msg_send![*panel_for_block, URL];
                    if url == nil {
                        None
                    } else {
                        let path: id = msg_send![url, path];
                        if path == nil {
                            None
                        } else {
                            Some(PathBuf::from(nsstring_to_str(path)))
                        }
                    }
                };

                if let Ok(mut callback) = callback_for_block.lock() {
                    if let Some(callback) = callback.take() {
                        callback(selected_path);
                    }
                }
            });
            let () = msg_send![*panel, beginWithCompletionHandler: &*block];
        }
    }

    fn pick_app_async(&self, callback: Box<dyn FnOnce(Option<PathBuf>) + 'static>) {
        unsafe {
            let _pool = NSAutoreleasePool::new(nil);
            let panel: id = msg_send![class!(NSOpenPanel), openPanel];
            let panel = StrongPtr::retain(panel);
            let () = msg_send![*panel, setCanChooseFiles: YES];
            let () = msg_send![*panel, setCanChooseDirectories: NO];
            let () = msg_send![*panel, setAllowsMultipleSelection: NO];
            let () = msg_send![*panel, setResolvesAliases: YES];
            let app_type = nsstring("app");
            let types: id = msg_send![class!(NSArray), arrayWithObject: *app_type];
            let () = msg_send![*panel, setAllowedFileTypes: types];
            let applications_dir = nsstring("/Applications");
            let dir_url: id =
                msg_send![class!(NSURL), fileURLWithPath: *applications_dir isDirectory: YES];
            let () = msg_send![*panel, setDirectoryURL: dir_url];
            let title = nsstring("Choose Application");
            let prompt = nsstring("Choose");
            let () = msg_send![*panel, setTitle: *title];
            let () = msg_send![*panel, setPrompt: *prompt];

            const NS_MODAL_RESPONSE_OK: NSInteger = 1;
            let callback = Arc::new(Mutex::new(Some(callback)));
            let callback_for_block = callback.clone();
            let panel_for_block = panel.clone();
            let block = RcBlock::new(move |result: NSInteger| {
                let selected_path = if result != NS_MODAL_RESPONSE_OK {
                    None
                } else {
                    let url: id = msg_send![*panel_for_block, URL];
                    if url == nil {
                        None
                    } else {
                        let path: id = msg_send![url, path];
                        if path == nil {
                            None
                        } else {
                            Some(PathBuf::from(nsstring_to_str(path)))
                        }
                    }
                };

                if let Ok(mut callback) = callback_for_block.lock() {
                    if let Some(callback) = callback.take() {
                        callback(selected_path);
                    }
                }
            });
            let () = msg_send![*panel, beginWithCompletionHandler: &*block];
        }
    }

    fn invalidate(&self) {
        Connection::with_window_inner(self.id, |inner| {
            inner.invalidate();
            Ok(())
        });
    }

    fn set_title(&self, title: &str) {
        let title = title.to_owned();
        Connection::with_window_inner(self.id, move |inner| {
            inner.set_title(&title);
            Ok(())
        });
    }

    fn set_icon(&self, image: Image) {
        let (width, height) = image.image_dimensions();
        let bitmap = BitmapRef::with_image(&image);

        unsafe {
            let size = NSSize::new(width as CGFloat, height as CGFloat);
            let ns_image: id = msg_send![
                NSImage::alloc(nil),
                initWithCGImage: bitmap.as_ptr()
                size: size
            ];

            if ns_image != nil {
                let ns_image = StrongPtr::new(ns_image);
                let _: BOOL = msg_send![*ns_image, setName:NSImageNameApplicationIcon];
                let app = NSApplication::sharedApplication(nil);
                app.setApplicationIconImage_(*ns_image);
            }
        }
    }

    fn set_titlebar_sidebar_button_visible(&self, visible: bool) {
        Connection::with_window_inner(self.id, move |inner| {
            inner.set_titlebar_sidebar_button_visible(visible);
            Ok(())
        });
    }

    fn set_window_level(&self, level: WindowLevel) {
        Connection::with_window_inner(self.id, move |inner| {
            inner.set_window_level(level);
            Ok(())
        });
    }

    fn set_inner_size(&self, width: usize, height: usize) {
        Connection::with_window_inner(self.id, move |inner| {
            inner.set_inner_size(width, height);
            if let Some(window_view) = WindowView::get_this(unsafe { &**inner.view }) {
                window_view
                    .inner
                    .borrow_mut()
                    .events
                    .dispatch(WindowEvent::SetInnerSizeCompleted);
            }
            Ok(())
        });
    }

    fn set_window_position(&self, coords: ScreenPoint) {
        Connection::with_window_inner(self.id, move |inner| {
            inner.set_window_position(coords);
            Ok(())
        });
    }

    fn request_drag_move(&self) {
        Connection::with_window_inner(self.id, move |inner| {
            inner.request_drag_move();
            Ok(())
        });
    }

    fn set_text_cursor_position(&self, cursor: Rect) {
        Connection::with_window_inner(self.id, move |inner| {
            inner.set_text_cursor_position(cursor);
            Ok(())
        });
    }

    fn set_native_text_input_snapshot(&self, snapshot: Option<NativeTextInputSnapshot>) {
        Connection::with_window_inner(self.id, move |inner| {
            inner.set_native_text_input_snapshot(snapshot);
            Ok(())
        });
    }

    fn show_text_definition(&self, text: &str, anchor: Rect) {
        let text = text.to_string();
        Connection::with_window_inner(self.id, move |inner| {
            inner.show_text_definition(&text, anchor);
            Ok(())
        });
    }

    fn text_check_capabilities(&self) -> TextCheckCapabilities {
        TextCheckCapabilities {
            spelling: true,
            suggestions: true,
            ignore: true,
            learn: true,
        }
    }

    fn request_text_check(&self, request: TextCheckRequest) -> Future<TextCheckResponse> {
        let mut promise = Promise::new();
        let future = promise
            .get_future()
            .expect("new text-check promise must have a future");
        let promise = Arc::new(Mutex::new(Some(promise)));
        Connection::with_window_inner(self.id, move |inner| {
            inner.request_text_check(request, promise);
            Ok(())
        });
        future
    }

    fn ignore_spelling_word(&self, document_id: &str, word: &str) {
        let document_id = document_id.to_string();
        let word = word.to_string();
        Connection::with_window_inner(self.id, move |inner| {
            inner.ignore_spelling_word(&document_id, &word);
            Ok(())
        });
    }

    fn learn_spelling_word(&self, word: &str) {
        let word = word.to_string();
        Connection::with_window_inner(self.id, move |inner| {
            inner.learn_spelling_word(&word);
            Ok(())
        });
    }

    fn get_clipboard(&self, _clipboard: Clipboard) -> Future<String> {
        Future::result(
            ClipboardContext::new()
                .read()
                .map_err(|e| anyhow!("Failed to get clipboard:{}", e)),
        )
    }

    fn get_clipboard_contents(&self, _clipboard: Clipboard) -> Future<ClipboardContents> {
        Future::result(
            ClipboardContext::new()
                .read_contents()
                .map_err(|e| anyhow!("Failed to get clipboard:{}", e)),
        )
    }

    fn set_clipboard(&self, _clipboard: Clipboard, text: String) {
        ClipboardContext::new().write(text).ok();
    }

    fn toggle_fullscreen(&self) {
        Connection::with_window_inner(self.id, move |inner| {
            inner.toggle_fullscreen();
            Ok(())
        });
    }

    fn maximize(&self) {
        Connection::with_window_inner(self.id, move |inner| {
            inner.maximize();
            Ok(())
        });
    }

    fn restore(&self) {
        Connection::with_window_inner(self.id, move |inner| {
            inner.restore();
            Ok(())
        });
    }

    fn set_resize_increments(&self, incr: ResizeIncrement) {
        Connection::with_window_inner(self.id, move |inner| {
            inner.set_resize_increments(incr);
            Ok(())
        });
    }

    fn config_did_change(&self, config: &ConfigHandle) {
        let config = config.clone();
        Connection::with_window_inner(self.id, move |inner| {
            inner.config_did_change(&config);
            Ok(())
        });
    }

    fn get_os_parameters(
        &self,
        config: &ConfigHandle,
        window_state: WindowState,
    ) -> anyhow::Result<Option<Parameters>> {
        // We implement this method primarily to provide Notch-avoidance for
        // systems with a notch.
        // We only need this for non-native full screen mode.

        let native_full_screen = {
            let style_mask = unsafe { NSWindow::styleMask(self.ns_window) };
            style_mask.contains(NSWindowStyleMask::NSFullScreenWindowMask)
        };

        let border_dimensions = if window_state.contains(WindowState::FULL_SCREEN)
            && !native_full_screen
            && !config.macos_fullscreen_extend_behind_notch
        {
            let main_screen = unsafe { NSScreen::mainScreen(nil) };
            let has_safe_area_insets: BOOL =
                unsafe { msg_send![main_screen, respondsToSelector: sel!(safeAreaInsets)] };
            if has_safe_area_insets == YES {
                #[derive(Debug)]
                struct NSEdgeInsets {
                    top: CGFloat,
                    left: CGFloat,
                    bottom: CGFloat,
                    right: CGFloat,
                }
                let insets: NSEdgeInsets = unsafe { msg_send![main_screen, safeAreaInsets] };
                log::trace!("{:?}", insets);

                let scale = unsafe {
                    let frame = NSScreen::frame(main_screen);
                    let backing_frame = NSScreen::convertRectToBacking_(main_screen, frame);
                    backing_frame.size.height / frame.size.height
                };

                let top = (insets.top.ceil() * scale) as usize;
                Some(Border {
                    top: ULength::new(top),
                    left: ULength::new(insets.left.ceil() as usize),
                    right: ULength::new(insets.right.ceil() as usize),
                    bottom: ULength::new(insets.bottom.ceil() as usize),
                    color: crate::color::LinearRgba::with_components(0., 0., 0., 1.),
                })
            } else {
                None
            }
        } else {
            None
        };

        Ok(Some(Parameters {
            title_bar: TitleBar {
                padding_left: ULength::new(0),
                padding_right: ULength::new(0),
                height: None,
                font_and_size: None,
            },
            border_dimensions,
        }))
    }
}

/// Convert from a macOS screen coordinate with the origin in the bottom left
/// to a pixel coordinate with its origin in the top left
fn cartesian_to_screen_point(cartesian: NSPoint) -> ScreenPoint {
    unsafe {
        let screens = NSScreen::screens(nil);
        let primary = screens.objectAtIndex(0);
        let frame = NSScreen::frame(primary);
        let backing_frame = NSScreen::convertRectToBacking_(primary, frame);
        let scale = backing_frame.size.height / frame.size.height;
        ScreenPoint::new(
            (cartesian.x * scale) as isize,
            ((frame.size.height - cartesian.y) * scale) as isize,
        )
    }
}

/// Convert from a pixel coordinate in the top left to a macOS screen
/// coordinate with its origin in the bottom left
fn screen_point_to_cartesian(point: ScreenPoint) -> NSPoint {
    unsafe {
        let screens = NSScreen::screens(nil);
        let primary = screens.objectAtIndex(0);
        let frame = NSScreen::frame(primary);
        let backing_frame = NSScreen::convertRectToBacking_(primary, frame);
        let scale = backing_frame.size.height / frame.size.height;
        NSPoint::new(
            point.x as f64 / scale,
            frame.size.height - (point.y as f64 / scale),
        )
    }
}

impl WindowInner {
    fn enable_opengl(&mut self) -> anyhow::Result<Rc<glium::backend::Context>> {
        if let Some(window_view) = WindowView::get_this(unsafe { &**self.view }) {
            window_view.inner.borrow_mut().enable_opengl()
        } else {
            anyhow::bail!("window invalid");
        }
    }

    fn is_fullscreen(&mut self) -> bool {
        if self.is_native_fullscreen() {
            true
        } else if let Some(window_view) = WindowView::get_this(unsafe { &**self.view }) {
            window_view.inner.borrow().fullscreen.is_some()
        } else {
            false
        }
    }

    fn apply_decorations(&mut self) {
        if !self.is_fullscreen() {
            apply_decorations_to_window(
                &self.window,
                self.config.window_decorations,
                self.config.integrated_title_button_style,
            );
            self.update_titlebar_sidebar_button();
        }
    }

    fn set_titlebar_sidebar_button_visible(&mut self, visible: bool) {
        self.titlebar_sidebar_button_visible = visible;
        if let Some(window_view) = WindowView::get_this(unsafe { &**self.view }) {
            window_view
                .inner
                .borrow_mut()
                .titlebar_sidebar_button_visible = visible;
        }
        self.update_titlebar_sidebar_button();
    }

    fn update_titlebar_sidebar_button(&mut self) {
        if self.titlebar_sidebar_button_visible && !self.is_fullscreen() {
            install_thinkterm_titlebar_sidebar_button(&self.window, *self.view);
        } else {
            remove_thinkterm_titlebar_sidebar_button(&self.window);
        }
    }

    fn toggle_native_fullscreen(&mut self) {
        unsafe {
            NSWindow::toggleFullScreen_(*self.window, nil);
        }
    }

    fn is_native_fullscreen(&self) -> bool {
        let style_mask = unsafe { NSWindow::styleMask(*self.window) };
        style_mask.contains(NSWindowStyleMask::NSFullScreenWindowMask)
    }

    /// If we were in native full screen mode, exit it and return true.
    /// Otherwise, return false
    fn exit_native_fullscreen(&mut self) -> bool {
        if self.is_native_fullscreen() {
            self.toggle_native_fullscreen();
            true
        } else {
            false
        }
    }

    /// If we were in simple full screen mode, exit it and return true.
    /// Otherwise, return false
    fn exit_simple_fullscreen(&mut self) -> bool {
        if let Some(window_view) = WindowView::get_this(unsafe { &**self.view }) {
            let is_fullscreen = window_view.inner.borrow().fullscreen.is_some();
            if is_fullscreen {
                self.toggle_simple_fullscreen();
            }
            is_fullscreen
        } else {
            false
        }
    }

    fn toggle_simple_fullscreen(&mut self) {
        let current_app = unsafe { NSApplication::sharedApplication(nil) };

        if let Some(window_view) = WindowView::get_this(unsafe { &**self.view }) {
            let fullscreen = window_view.inner.borrow_mut().fullscreen.take();
            match fullscreen {
                Some(saved_rect) => unsafe {
                    // Restore prior dimensions
                    self.window.orderOut_(nil);
                    apply_decorations_to_window(
                        &self.window,
                        self.config.window_decorations,
                        self.config.integrated_title_button_style,
                    );
                    self.window.setFrame_display_(saved_rect, YES);
                    self.window.makeKeyAndOrderFront_(nil);
                    self.window.setOpaque_(NO);
                    current_app.setPresentationOptions_(
                        NSApplicationPresentationOptions::NSApplicationPresentationDefault,
                    );
                },
                None => unsafe {
                    // Go full screen
                    let saved_rect = NSWindow::frame(*self.window);
                    window_view
                        .inner
                        .borrow_mut()
                        .fullscreen
                        .replace(saved_rect);

                    let main_screen = NSScreen::mainScreen(nil);
                    let screen_rect = NSScreen::frame(main_screen);

                    self.window.orderOut_(nil);
                    self.window
                        .setStyleMask_(NSWindowStyleMask::NSBorderlessWindowMask);
                    self.window.setFrame_display_(screen_rect, YES);
                    self.window.makeKeyAndOrderFront_(nil);
                    self.window.setOpaque_(YES);
                    current_app.setPresentationOptions_(
                        NSApplicationPresentationOptions:: NSApplicationPresentationAutoHideMenuBar
                            | NSApplicationPresentationOptions::NSApplicationPresentationAutoHideDock
                    );
                },
            }
        }
    }

    fn update_window_shadow(&mut self) {
        let is_opaque = if self.config.window_background_opacity >= 1.0 {
            YES
        } else {
            NO
        };
        unsafe {
            self.window.setOpaque_(is_opaque);
            // when transparent, also turn off the window shadow,
            // because having the shadow enabled seems to correlate
            // with ghostly remnants see:
            // https://github.com/wezterm/wezterm/issues/310.
            // But allow overriding the shadows independent of opacity as well:
            // <https://github.com/wezterm/wezterm/issues/2669>
            let shadow = if self
                .config
                .window_decorations
                .contains(WindowDecorations::MACOS_FORCE_ENABLE_SHADOW)
            {
                YES
            } else if self
                .config
                .window_decorations
                .contains(WindowDecorations::MACOS_FORCE_DISABLE_SHADOW)
            {
                NO
            } else {
                is_opaque
            };
            self.window.setHasShadow_(shadow);
        }
    }

    fn update_titlebar_background(&self) {
        unsafe {
            if let Some(titlebar_view_container) = get_titlebar_view_container(&self.window) {
                let titlebar_view_container_id = titlebar_view_container.load();
                set_view_background_color(
                    *titlebar_view_container_id,
                    RgbaColor::from(SrgbaTuple(0.0, 0.0, 0.0, 0.0)),
                );
                set_standard_window_buttons_visible(&self.window, true);
            } else {
                log::trace!("failed to get titlebar view container from window");
            }
        }
    }

    fn update_window_background_blur(&mut self) {
        unsafe {
            CGSSetWindowBackgroundBlurRadius(
                CGSMainConnectionID(),
                self.window.windowNumber(),
                self.config.macos_window_background_blur,
            );
        }
    }
}

impl WindowInner {
    fn show(&mut self) {
        unsafe {
            let current_app = NSRunningApplication::currentApplication(nil);
            current_app.activateWithOptions_(NSApplicationActivateIgnoringOtherApps);

            // Stupid hack: adjust the window style mask and set it back
            // to what it was.
            // Without this, the CAMetalLayer used by webgpu seems to get
            // stuck with a scale factor of 2 despite us having configured 1.
            self.window
                .setStyleMask_(NSWindowStyleMask::NSBorderlessWindowMask);

            apply_decorations_to_window(
                &self.window,
                self.config.window_decorations,
                self.config.integrated_title_button_style,
            );

            self.update_titlebar_background();
            self.update_titlebar_sidebar_button();

            self.window.makeKeyAndOrderFront_(nil);
            self.update_titlebar_background();
        }
    }

    fn close(&mut self) {
        unsafe {
            self.window.close();
        }
    }

    fn focus(&mut self) {
        unsafe {
            self.window.makeKeyAndOrderFront_(nil);
        }
    }

    fn hide(&mut self) {
        unsafe {
            NSWindow::miniaturize_(*self.window, *self.window);
            // We could literally set it invisible like this, but
            // then there is no UI to make it visible again later.
            //let () = msg_send![*self.window, setIsVisible: NO];
        }
    }

    fn set_cursor(&mut self, cursor: Option<MouseCursor>) {
        unsafe {
            let ns_cursor_cls = class!(NSCursor);

            // Remember the requested cursor on the view so that our
            // `resetCursorRects` override can re-assert it. macOS aggressively
            // resets the cursor to the default arrow as part of its own
            // cursor-management cycle; this is especially visible while the
            // main thread is busy painting heavy terminal output, because our
            // `mouseMoved:`-driven `set` calls get coalesced/delayed and the
            // arrow shows through, making the I-beam appear to flicker.
            // Registering a cursor rect (see `reset_cursor_rects`) lets AppKit
            // draw our cursor over the view instead of falling back to arrow.
            //
            // Avoid borrowing the view's `Inner` here: `set_cursor` can be
            // reached synchronously from inside an event dispatch that already
            // holds that borrow, so we stash the value in an ivar instead.
            // Only rebuild the cursor rects when the cursor actually changes.
            // `set_cursor` runs on every mouse move, and
            // `invalidateCursorRectsForView:` makes AppKit re-route the rects
            // through the window server (`routeCursorRect` ->
            // `_NSFindWindowUnderMouse` -> SLS* IPC) synchronously on the main
            // thread. Sampled during a divider drag, that routing ate 30-45%
            // of the drag's wall time and serialized mouse-event delivery --
            // the whole app felt like a remote session. The registered rect
            // stays valid while the cursor kind is unchanged, so skipping the
            // invalidation loses nothing.
            let code = cursor_to_code(cursor);
            let prev: i64 = *(**self.view).get_ivar::<i64>(CURSOR_IVAR);
            if prev != code {
                (**self.view).set_ivar::<i64>(CURSOR_IVAR, code);
                // Ask AppKit to rebuild the cursor rects so the change above
                // takes effect for its own cursor management on the next event.
                let () = msg_send![*self.window, invalidateCursorRectsForView: *self.view];
            }

            if let Some(cursor) = cursor {
                // Unconditionally apply the requested cursor, as there are
                // cases where macOS can decide to change the cursor to something
                // that we don't know about.
                let instance = ns_cursor_instance(cursor);
                let () = msg_send![ns_cursor_cls, setHiddenUntilMouseMoves: NO];
                let () = msg_send![instance, set];
            } else {
                let () = msg_send![ns_cursor_cls, setHiddenUntilMouseMoves: YES];
            }
        }
    }

    fn show_context_menu(&mut self, coords: Point, items: Vec<ContextMenuItem>) {
        if items.is_empty() {
            return;
        }

        fn add_context_menu_items(menu: &Menu, view: id, items: Vec<ContextMenuItem>) -> bool {
            let mut has_items = false;
            for item in items {
                match item {
                    ContextMenuItem::Item {
                        label,
                        icon,
                        action,
                        checked,
                        enabled,
                        submenu,
                    } => {
                        let has_submenu = !submenu.is_empty();
                        let menu_item = MenuItem::new_with(
                            &label,
                            if has_submenu {
                                None
                            } else {
                                Some(sel!(weztermPerformKeyAssignment:))
                            },
                            "",
                        );
                        if let Some(icon) = icon {
                            menu_item.set_context_menu_icon(icon);
                        }
                        menu_item.set_checked(checked);
                        menu_item.set_enabled(enabled);
                        if has_submenu {
                            let child_menu = Menu::new_with_title(&label);
                            child_menu.set_autoenables_items(false);
                            if add_context_menu_items(&child_menu, view, submenu) {
                                menu_item.set_sub_menu(&child_menu);
                                menu.add_item(&menu_item);
                                has_items = true;
                            }
                        } else {
                            menu_item.set_target(view);
                            menu_item
                                .set_represented_item(RepresentedItem::ContextMenuAction(action));
                            menu.add_item(&menu_item);
                            has_items = true;
                        }
                    }
                    ContextMenuItem::SectionHeader { label } => {
                        // Falls back to a disabled item where the system has no
                        // section-header constructor; with automatic enabling
                        // off that still renders as grey, unhighlightable text.
                        let menu_item = MenuItem::new_section_header(&label).unwrap_or_else(|| {
                            let item = MenuItem::new_with(&label, None, "");
                            item.set_enabled(false);
                            item
                        });
                        menu.add_item(&menu_item);
                    }
                    ContextMenuItem::Separator => {
                        if has_items {
                            menu.add_item(&MenuItem::new_separator());
                        }
                    }
                }
            }
            has_items
        }

        let menu = Menu::new_with_title("");
        menu.set_autoenables_items(false);
        let has_items = add_context_menu_items(&menu, *self.view, items);

        if !has_items {
            return;
        }

        // popUpMenuPositioningItem runs a nested event loop that continues
        // to service the spawn queue.  We are called via with_window_inner,
        // which holds the window RefCell borrow for the duration of the
        // callback; popping the menu up while that borrow is held causes any
        // other with_window_inner callback dispatched during menu tracking
        // to panic with "RefCell already borrowed".  Defer the blocking
        // pop-up until after our caller releases the borrow.
        let view = self.view.clone();
        promise::spawn::spawn(async move {
            let selected = unsafe {
                let frame = NSView::frame(*view as *mut _);
                let backing_frame = NSView::convertRectToBacking(*view as *mut _, frame);
                let scale = if frame.size.width > 0.0 {
                    backing_frame.size.width / frame.size.width
                } else {
                    1.0
                };

                menu.pop_up_at(*view, coords.x as f64 / scale, coords.y as f64 / scale)
            };
            // Cocoa dispatches a selected action synchronously while tracking
            // the menu. That action may open another native menu and install
            // new pending confirmation state before this call returns, so the
            // original menu must only cancel state when it closed *without*
            // a selection (Escape or a click outside).
            if !selected {
                if let Some(window_view) = WindowView::get_this(unsafe { &**view }) {
                    window_view
                        .inner
                        .borrow_mut()
                        .events
                        .dispatch(WindowEvent::ContextMenuDismissed);
                }
            }
        })
        .detach();
    }

    fn invalidate(&mut self) {
        unsafe {
            let () = msg_send![*self.view, setNeedsDisplay: YES];
            if let Some(window_view) = WindowView::get_this(&**self.view) {
                window_view.invalidated.set(true);
            }
        }
    }
    fn set_title(&mut self, title: &str) {
        let title = nsstring(title);
        unsafe {
            NSWindow::setTitle_(*self.window, *title);
        }
    }

    fn set_window_level(&mut self, level: WindowLevel) {
        unsafe {
            NSWindow::setLevel_(*self.window, window_level_to_nswindow_level(level));
            // Dispatch a resize event with the updated window state
            WindowView::did_resize(&mut **self.view, sel!(windowDidResize:), nil);
        }
    }

    fn set_inner_size(&mut self, width: usize, height: usize) {
        unsafe {
            let frame = NSView::frame(*self.view as *mut _);
            let backing_frame = NSView::convertRectToBacking(*self.view as *mut _, frame);
            let scale = backing_frame.size.width / frame.size.width;

            NSWindow::setContentSize_(
                *self.window,
                NSSize::new(width as f64 / scale, height as f64 / scale),
            );

            // setContentSize_ doesn't explicitly invalidate,
            // so we need to do it ourselves
            self.invalidate();
        }
    }

    fn set_window_position(&self, coords: ScreenPoint) {
        set_window_position(*self.window, coords);
    }

    fn request_drag_move(&self) {
        unsafe {
            let app = NSApplication::sharedApplication(nil);
            let event: id = msg_send![app, currentEvent];
            if event != nil {
                let () = msg_send![*self.window, performWindowDragWithEvent: event];
            }
        }
    }

    fn set_text_cursor_position(&mut self, cursor: Rect) {
        if let Some(window_view) = WindowView::get_this(unsafe { &**self.view }) {
            window_view.inner.borrow_mut().text_cursor_position = cursor;
        }
        if self.config.use_ime {
            unsafe {
                let input_context: id = msg_send![&**self.view, inputContext];
                let () = msg_send![input_context, invalidateCharacterCoordinates];
            }
        }
    }

    fn set_native_text_input_snapshot(&mut self, snapshot: Option<NativeTextInputSnapshot>) {
        if let Some(window_view) = WindowView::get_this(unsafe { &**self.view }) {
            window_view.inner.borrow_mut().native_text_input_snapshot = snapshot;
        }
        if self.config.use_ime {
            unsafe {
                let input_context: id = msg_send![&**self.view, inputContext];
                let () = msg_send![input_context, invalidateCharacterCoordinates];
            }
        }
    }

    fn show_text_definition(&mut self, text: &str, anchor: Rect) {
        if text.trim().is_empty() {
            return;
        }
        unsafe {
            let frame = NSView::frame(*self.view as *mut _);
            let backing_frame = NSView::convertRectToBacking(*self.view as *mut _, frame);
            let scale = if frame.size.width > 0.0 {
                backing_frame.size.width / frame.size.width
            } else {
                1.0
            };
            let attributed: id = msg_send![class!(NSAttributedString), alloc];
            let attributed: id = msg_send![attributed, initWithString:*nsstring(text)];
            let point = NSPoint::new(
                anchor.origin.x as f64 / scale,
                anchor.origin.y as f64 / scale,
            );
            let (): () = msg_send![
                &**self.view,
                showDefinitionForAttributedString: attributed
                atPoint: point
            ];
            let (): () = msg_send![attributed, release];
        }
    }

    fn request_text_check(
        &mut self,
        request: TextCheckRequest,
        promise: Arc<Mutex<Option<Promise<TextCheckResponse>>>>,
    ) {
        unsafe {
            let _pool = NSAutoreleasePool::new(nil);
            let checker: id = msg_send![class!(NSSpellChecker), sharedSpellChecker];
            let ns_text = nsstring(&request.text);
            let ns_text_for_block = ns_text.clone();
            let request_text = request.text.clone();
            let request_id = request.request_id;
            let document_tag = spell_document_tag(&request.document_id);
            let promise_for_block = Arc::clone(&promise);
            let block = RcBlock::new(
                move |_sequence: NSInteger,
                      results: *mut objc2::runtime::AnyObject,
                      _orthography: *mut objc2::runtime::AnyObject,
                      _word_count: NSInteger| {
                    let results = results.cast::<Object>();
                    let mut issues = Vec::new();
                    if results != nil {
                        let count: NSUInteger = msg_send![results, count];
                        for index in 0..count {
                            let result: id = msg_send![results, objectAtIndex:index];
                            let range: NSRange = msg_send![result, range];
                            let utf16_start = range.0.location as usize;
                            let utf16_end = utf16_start.saturating_add(range.0.length as usize);
                            let start = byte_offset_for_utf16(&request_text, utf16_start);
                            let end = byte_offset_for_utf16(&request_text, utf16_end).max(start);
                            if start == end || end > request_text.len() {
                                continue;
                            }

                            let guesses: id = msg_send![
                                checker,
                                guessesForWordRange: range
                                inString: *ns_text_for_block
                                language: nil
                                inSpellDocumentWithTag: document_tag
                            ];
                            let mut suggestions = Vec::new();
                            if guesses != nil {
                                let guess_count: NSUInteger = msg_send![guesses, count];
                                for guess_index in 0..guess_count.min(5) {
                                    let guess: id = msg_send![guesses, objectAtIndex:guess_index];
                                    let guess = nsstring_to_str(guess).to_string();
                                    if !suggestions.contains(&guess) {
                                        suggestions.push(guess);
                                    }
                                }
                            }
                            issues.push(TextCheckIssue {
                                range: start..end,
                                suggestions,
                            });
                        }
                    }
                    if let Ok(mut promise) = promise_for_block.lock() {
                        if let Some(mut promise) = promise.take() {
                            promise.ok(TextCheckResponse { request_id, issues });
                        }
                    }
                },
            );
            const NS_TEXT_CHECKING_TYPE_SPELLING: NSUInteger = 1 << 1;
            let full_range = NSRange::new(0, request.text.encode_utf16().count() as u64);
            let _: NSInteger = msg_send![
                checker,
                requestCheckingOfString: *ns_text
                range: full_range
                types: NS_TEXT_CHECKING_TYPE_SPELLING
                options: nil
                inSpellDocumentWithTag: document_tag
                completionHandler: &*block
            ];
        }
    }

    fn ignore_spelling_word(&mut self, document_id: &str, word: &str) {
        if word.trim().is_empty() {
            return;
        }
        unsafe {
            let checker: id = msg_send![class!(NSSpellChecker), sharedSpellChecker];
            let word = nsstring(word);
            let (): () = msg_send![
                checker,
                ignoreWord: *word
                inSpellDocumentWithTag: spell_document_tag(document_id)
            ];
        }
    }

    fn learn_spelling_word(&mut self, word: &str) {
        if word.trim().is_empty() {
            return;
        }
        unsafe {
            let checker: id = msg_send![class!(NSSpellChecker), sharedSpellChecker];
            let word = nsstring(word);
            let (): () = msg_send![checker, learnWord: *word];
        }
    }

    fn is_zoomed(&self) -> bool {
        unsafe { msg_send![*self.window, isZoomed] }
    }

    fn maximize(&mut self) {
        if !self.is_zoomed() {
            unsafe {
                NSWindow::zoom_(*self.window, nil);
            }
        }
    }

    fn restore(&mut self) {
        if self.is_zoomed() {
            unsafe {
                NSWindow::zoom_(*self.window, nil);
            }
        }
    }

    fn toggle_fullscreen(&mut self) {
        let native_fullscreen = self.config.native_macos_fullscreen_mode;

        // If they changed their config since going full screen, be sure
        // to undo whichever fullscreen mode they had active rather than
        // trying to undo the one they have configured.

        if native_fullscreen {
            if !self.exit_simple_fullscreen() {
                self.toggle_native_fullscreen();
            }
        } else {
            if !self.exit_native_fullscreen() {
                self.toggle_simple_fullscreen();
            }
        }
    }

    fn set_resize_increments(&self, incr: ResizeIncrement) {
        let min_width = incr.base_width + incr.x;
        let min_height = incr.base_height + incr.y;
        unsafe {
            self.window
                .setResizeIncrements_(NSSize::new(incr.x.into(), incr.y.into()));
            let () = msg_send![
                *self.window,
                setContentMinSize: NSSize::new(min_width.into(), min_height.into())
            ];
        }
    }

    fn config_did_change(&mut self, config: &ConfigHandle) {
        let dpi_changed =
            self.config.dpi != config.dpi || self.config.dpi_by_screen != config.dpi_by_screen;

        self.config = config.clone();
        if let Some(window_view) = WindowView::get_this(unsafe { &**self.view }) {
            let mut inner = window_view.inner.borrow_mut();
            inner.config = config.clone();
            if dpi_changed {
                inner.screen_changed = true;
            }
        }
        self.update_window_shadow();
        self.update_window_background_blur();
        self.update_titlebar_background();
        self.apply_decorations();
    }
}

fn effective_decorations(
    mut decorations: WindowDecorations,
    integrated_title_button_style: IntegratedTitleButtonStyle,
) -> WindowDecorations {
    if integrated_title_button_style != IntegratedTitleButtonStyle::MacOsNative {
        decorations.remove(WindowDecorations::INTEGRATED_BUTTONS);
    }
    decorations
}

fn apply_decorations_to_window(
    window: &StrongPtr,
    decorations: WindowDecorations,
    integrated_title_button_style: IntegratedTitleButtonStyle,
) {
    let mask = decoration_to_mask(decorations, integrated_title_button_style);
    unsafe {
        window.setStyleMask_(mask);

        set_standard_window_buttons_visible(window, true);

        window.setTitleVisibility_(appkit::NSWindowTitleVisibility::NSWindowTitleHidden);

        window.setTitlebarAppearsTransparent_(YES);
    }
}

fn install_thinkterm_titlebar_sidebar_button(window: &StrongPtr, target: id) {
    unsafe {
        let Some(titlebar_view_container) = get_titlebar_view_container(window) else {
            return;
        };
        let titlebar_view_container = titlebar_view_container.load();
        if titlebar_view_container.is_null() {
            return;
        }
        let titlebar_view_container_id = *titlebar_view_container;
        if let Some(button) = thinkterm_titlebar_sidebar_button(&titlebar_view_container) {
            position_thinkterm_titlebar_sidebar_button(window, titlebar_view_container_id, button);
            return;
        }

        let button: id = msg_send![
            class!(NSButton),
            buttonWithTitle: *nsstring("")
            target: target
            action: sel!(thinktermToggleWorkspaceSidebar:)
        ];
        if button.is_null() {
            return;
        }

        position_thinkterm_titlebar_sidebar_button(window, titlebar_view_container_id, button);
        let () = msg_send![button, setBordered: NO];
        let () = msg_send![button, setBezelStyle: 0isize];
        let () = msg_send![button, setImagePosition: 1isize];
        let () = msg_send![button, setTag: THINKTERM_TITLEBAR_SIDEBAR_BUTTON_TAG];

        let image_class = class!(NSImage);
        let supports_symbol_images: BOOL = msg_send![
            image_class,
            respondsToSelector: sel!(imageWithSystemSymbolName:accessibilityDescription:)
        ];
        if supports_symbol_images == YES {
            let image: id = msg_send![
                image_class,
                imageWithSystemSymbolName: *nsstring("sidebar.left")
                accessibilityDescription: *nsstring("Toggle sidebar")
            ];
            if !image.is_null() {
                let () = msg_send![button, setImage: image];
            } else {
                let () = msg_send![button, setTitle: *nsstring("▣")];
            }
        } else {
            let () = msg_send![button, setTitle: *nsstring("▣")];
        }

        let () = msg_send![titlebar_view_container_id, addSubview: button];

        // A tracking area owned by the WindowView, so hovering the native
        // button reaches the GUI as WorkspaceSidebarButtonHover — this is
        // what lets the hover-reveal treat it like the painted toggles.
        let info: id = msg_send![class!(NSMutableDictionary), new];
        let marker_key = nsstring(THINKTERM_TITLEBAR_SIDEBAR_BUTTON_TRACKING_KEY);
        let marker_val = nsstring("1");
        let () = msg_send![info, setObject: *marker_val forKey: *marker_key];
        // NSTrackingMouseEnteredAndExited | NSTrackingActiveInActiveApp
        // | NSTrackingInVisibleRect
        let options: NSUInteger = 0x01 | 0x40 | 0x200;
        let area: id = msg_send![class!(NSTrackingArea), alloc];
        let area: id = msg_send![
            area,
            initWithRect: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0))
            options: options
            owner: target
            userInfo: info
        ];
        let () = msg_send![button, addTrackingArea: area];
        let () = msg_send![area, release];
        let () = msg_send![info, release];
    }
}

/// Whether this enter/exit event came from the titlebar sidebar button's
/// tracking area (as opposed to the content view's own).
fn event_is_sidebar_button_tracking(nsevent: id) -> bool {
    unsafe {
        let area: id = msg_send![nsevent, trackingArea];
        if area.is_null() {
            return false;
        }
        let info: id = msg_send![area, userInfo];
        if info.is_null() {
            return false;
        }
        let key = nsstring(THINKTERM_TITLEBAR_SIDEBAR_BUTTON_TRACKING_KEY);
        let val: id = msg_send![info, objectForKey: *key];
        !val.is_null()
    }
}

fn position_thinkterm_titlebar_sidebar_button(
    window: &StrongPtr,
    titlebar_view_container_id: id,
    button: id,
) {
    unsafe {
        let titlebar_frame = NSView::frame(titlebar_view_container_id);
        let zoom_button = window.standardWindowButton_(appkit::NSWindowButton::NSWindowZoomButton);
        let zoom_button_hidden = if zoom_button.is_null() {
            false
        } else {
            let hidden: BOOL = msg_send![zoom_button, isHidden];
            hidden == YES
        };
        let x = if zoom_button.is_null() {
            THINKTERM_TITLEBAR_SIDEBAR_BUTTON_FALLBACK_X
        } else {
            let zoom_frame = NSView::frame(zoom_button);
            if zoom_button_hidden && zoom_frame.size.width <= 0.0 {
                THINKTERM_TITLEBAR_SIDEBAR_BUTTON_FALLBACK_X
            } else {
                zoom_frame.origin.x + zoom_frame.size.width + THINKTERM_TITLEBAR_SIDEBAR_BUTTON_GAP
            }
        };
        let y =
            ((titlebar_frame.size.height - THINKTERM_TITLEBAR_SIDEBAR_BUTTON_SIZE) / 2.0).max(0.0);
        let frame = NSRect::new(
            NSPoint::new(x, y),
            NSSize::new(
                THINKTERM_TITLEBAR_SIDEBAR_BUTTON_SIZE,
                THINKTERM_TITLEBAR_SIDEBAR_BUTTON_SIZE,
            ),
        );
        let () = msg_send![button, setFrame: frame];
    }
}

fn remove_thinkterm_titlebar_sidebar_button(window: &StrongPtr) {
    unsafe {
        let Some(titlebar_view_container) = get_titlebar_view_container(window) else {
            return;
        };
        let titlebar_view_container = titlebar_view_container.load();
        if titlebar_view_container.is_null() {
            return;
        }

        let Some(button) = thinkterm_titlebar_sidebar_button(&titlebar_view_container) else {
            return;
        };
        let () = msg_send![button, removeFromSuperview];
    }
}

fn thinkterm_titlebar_sidebar_button(titlebar_view_container: &StrongPtr) -> Option<id> {
    unsafe {
        let Some(subviews) = get_view_subviews(titlebar_view_container) else {
            return None;
        };
        let subviews = subviews.load();
        let count = subviews.count();

        for i in 0..count {
            let subview: id = subviews.objectAtIndex(i);
            if subview.is_null() {
                continue;
            }

            let responds_to_tag: BOOL = msg_send![subview, respondsToSelector: sel!(tag)];
            if responds_to_tag != YES {
                continue;
            }

            let tag: NSInteger = msg_send![subview, tag];
            if tag == THINKTERM_TITLEBAR_SIDEBAR_BUTTON_TAG {
                return Some(subview);
            }
        }

        None
    }
}

fn decoration_to_mask(
    decorations: WindowDecorations,
    integrated_title_button_style: IntegratedTitleButtonStyle,
) -> NSWindowStyleMask {
    let decorations = effective_decorations(decorations, integrated_title_button_style);
    let decorations = decorations.difference(
        WindowDecorations::MACOS_FORCE_DISABLE_SHADOW
            | WindowDecorations::MACOS_FORCE_ENABLE_SHADOW,
    );
    if decorations == WindowDecorations::TITLE | WindowDecorations::RESIZE {
        NSWindowStyleMask::NSTitledWindowMask
            | NSWindowStyleMask::NSClosableWindowMask
            | NSWindowStyleMask::NSMiniaturizableWindowMask
            | NSWindowStyleMask::NSResizableWindowMask
            | NSWindowStyleMask::NSFullSizeContentViewWindowMask
    } else if decorations
        == WindowDecorations::MACOS_FORCE_SQUARE_CORNERS | WindowDecorations::RESIZE
    {
        NSWindowStyleMask::NSTitledWindowMask
            | NSWindowStyleMask::NSClosableWindowMask
            | NSWindowStyleMask::NSMiniaturizableWindowMask
            | NSWindowStyleMask::NSResizableWindowMask
            | NSWindowStyleMask::NSFullSizeContentViewWindowMask
    } else if decorations == WindowDecorations::RESIZE
        || decorations == WindowDecorations::INTEGRATED_BUTTONS
        || decorations == WindowDecorations::INTEGRATED_BUTTONS | WindowDecorations::RESIZE
    {
        NSWindowStyleMask::NSTitledWindowMask
            | NSWindowStyleMask::NSClosableWindowMask
            | NSWindowStyleMask::NSMiniaturizableWindowMask
            | NSWindowStyleMask::NSResizableWindowMask
            | NSWindowStyleMask::NSFullSizeContentViewWindowMask
    } else if decorations == WindowDecorations::NONE {
        NSWindowStyleMask::NSTitledWindowMask
            | NSWindowStyleMask::NSClosableWindowMask
            | NSWindowStyleMask::NSMiniaturizableWindowMask
            | NSWindowStyleMask::NSFullSizeContentViewWindowMask
    } else if decorations == WindowDecorations::TITLE {
        NSWindowStyleMask::NSTitledWindowMask
            | NSWindowStyleMask::NSClosableWindowMask
            | NSWindowStyleMask::NSMiniaturizableWindowMask
    } else if decorations == WindowDecorations::MACOS_FORCE_SQUARE_CORNERS {
        NSWindowStyleMask::NSTitledWindowMask
            | NSWindowStyleMask::NSClosableWindowMask
            | NSWindowStyleMask::NSMiniaturizableWindowMask
            | NSWindowStyleMask::NSFullSizeContentViewWindowMask
    } else {
        NSWindowStyleMask::NSTitledWindowMask
            | NSWindowStyleMask::NSClosableWindowMask
            | NSWindowStyleMask::NSMiniaturizableWindowMask
            | NSWindowStyleMask::NSResizableWindowMask
    }
}

unsafe fn get_view_class_name(id: id) -> Option<String> {
    if id.is_null() {
        return None;
    }

    let class_name: id = msg_send![id, className];

    if class_name.is_null() {
        return None;
    }

    let cstr = CStr::from_ptr(class_name.UTF8String()).to_str();

    match cstr {
        Ok(s) => Some(s.to_string()),
        Err(_) => None,
    }
}

fn get_titlebar_view_container(window: &StrongPtr) -> Option<WeakPtr> {
    // The view container for the titlebar on macos is found next to the primary window view
    // so we need to traverse up to the super view to find it
    let super_view = get_view_superview(window)?;

    let sub_views = get_view_subviews(&super_view.load())?;

    let count = unsafe { sub_views.load().count() };

    for i in 0..count {
        let sub_view: id = unsafe { sub_views.load().objectAtIndex(i) };

        if sub_view.is_null() {
            continue;
        }

        let class_name = unsafe { get_view_class_name(sub_view)? };

        if class_name == TITLEBAR_VIEW_NAME {
            let titlebar_view = unsafe { WeakPtr::new(sub_view) };
            return Some(titlebar_view);
        }
    }

    None
}

fn get_view_superview(view: &StrongPtr) -> Option<WeakPtr> {
    let super_view_id: id = unsafe { msg_send![view.contentView(), superview] };

    if super_view_id.is_null() {
        return None;
    }

    let super_view = unsafe { WeakPtr::new(super_view_id) };

    Some(super_view)
}

fn get_view_subviews(view: &StrongPtr) -> Option<WeakPtr> {
    let sub_views_id: id = unsafe { msg_send![**view, subviews] };
    if sub_views_id.is_null() {
        return None;
    }

    let sub_views = unsafe { WeakPtr::new(sub_views_id) };
    Some(sub_views)
}

#[derive(Debug)]
struct DeadKeyState {
    /// The private dead key state preserved from UCKeyTranslate
    dead_state: u32,
}

struct Inner {
    events: WindowEventSender,
    view_id: Option<WeakPtr>,
    window: Option<WeakPtr>,
    titlebar_sidebar_button_visible: bool,
    screen_changed: bool,
    window_id: usize,
    gl_context_pair: Option<GlContextPair>,
    text_cursor_position: Rect,
    tracking_rect_tag: NSInteger,
    /// Backing size the live tracking rect was registered for, so
    /// `updateTrackingAreas` can skip a no-op rebuild. See the comment there.
    tracking_rect_size: (f64, f64),
    hscroll_remainder: f64,
    vscroll_remainder: f64,
    last_wheel: Instant,
    last_repaint_time: Option<Instant>,
    /// We use this to avoid double-emitting events when
    /// procesing key-up events.
    key_is_down: Option<bool>,

    /// First in a dead-key sequence
    dead_pending: Option<DeadKeyState>,

    /// When using simple fullscreen mode, this tracks
    /// the window dimensions that need to be restored
    fullscreen: Option<NSRect>,

    config: ConfigHandle,

    /// Used to signal when IME really just swallowed a key
    ime_state: ImeDisposition,
    /// Captures the last event that had ImeDisposition::Acted,
    /// so that we can use it to generate a repeat in the cases
    /// where the IME mysteriously swallows repeats but only
    /// for certain keys.
    ime_last_event: Option<KeyEvent>,

    /// Whether we're in live resize
    live_resizing: bool,

    ime_text: String,
    native_text_input_snapshot: Option<NativeTextInputSnapshot>,
}

#[repr(C)]
pub struct __InputSource {
    _dummy: i32,
}
pub type InputSourceRef = *const __InputSource;

declare_TCFType!(InputSource, InputSourceRef);
impl_TCFType!(InputSource, InputSourceRef, TISInputSourceGetTypeID);

#[repr(C)]
struct UCKeyboardLayout {
    _dummy: i32,
}

type UniCharCount = std::os::raw::c_ulong;

/// key is going down
#[allow(non_upper_case_globals)]
const kUCKeyActionDown: u16 = 0;
/// key is going up
#[allow(non_upper_case_globals, dead_code)]
const kUCKeyActionUp: u16 = 1;
/// auto-key down
#[allow(non_upper_case_globals, dead_code)]
const kUCKeyActionAutoKey: u16 = 2;
/// get information for key display (as in Key Caps)
#[allow(non_upper_case_globals)]
const kUCKeyActionDisplay: u16 = 3;

extern "C" {
    fn TISInputSourceGetTypeID() -> CFTypeID;
    fn TISCopyCurrentKeyboardInputSource() -> InputSourceRef;
    fn TISGetInputSourceProperty(source: InputSourceRef, propertyKey: CFStringRef) -> CFDataRef;

    static kTISPropertyUnicodeKeyLayoutData: CFStringRef;

    fn UCKeyTranslate(
        layout: *const UCKeyboardLayout,
        virtualKeyCode: u16,
        keyAction: u16,
        modifierKeyState: u32,
        keyboardType: u32,
        keyTranslateOptions: u32,
        deadKeyState: *mut u32,
        maxStringLength: UniCharCount,
        actualStringLength: *mut UniCharCount,
        unicodeString: *mut UniChar,
    ) -> u32;

    fn LMGetKbdType() -> u8;
}

#[derive(Debug)]
enum TranslateStatus {
    Composing(String),
    Composed(String),
    NotDead,
}

/// Represents the current keyboard layout.
/// Holds state needed to perform keymap translation.
struct Keyboard {
    _kbd: InputSource,
    layout_data: Option<CFData>,
}

/// Slightly more intelligible parameters for keymap translation
struct TranslateParams {
    virtual_key_code: u16,
    modifier_flags: NSEventModifierFlags,
    dead_state: u32,
    ignore_dead_keys: bool,
    display: bool,
}

/// The results of a keymap translation
#[derive(Debug)]
struct TranslateResults {
    dead_state: u32,
    text: String,
}

impl Keyboard {
    pub fn new() -> Self {
        let _kbd =
            unsafe { InputSource::wrap_under_create_rule(TISCopyCurrentKeyboardInputSource()) };

        let layout_data = unsafe {
            let data = TISGetInputSourceProperty(
                _kbd.as_concrete_TypeRef(),
                kTISPropertyUnicodeKeyLayoutData,
            );
            if data.is_null() {
                None
            } else {
                Some(CFData::wrap_under_get_rule(data))
            }
        };
        Self { _kbd, layout_data }
    }

    /// A wrapper around UCKeyTranslate
    pub fn translate(&self, params: TranslateParams) -> anyhow::Result<TranslateResults> {
        let layout_data = match &self.layout_data {
            Some(data) => unsafe {
                CFDataGetBytePtr(data.as_concrete_TypeRef()) as *const UCKeyboardLayout
            },
            None => std::ptr::null(),
        };

        let modifier_key_state: u32 = (params.modifier_flags.bits() >> 16) as u32 & 0xFF;

        let kbd_type = unsafe { LMGetKbdType() } as _;
        #[allow(non_upper_case_globals)]
        const kUCKeyTranslateNoDeadKeysBit: u32 = 0;

        let mut unicode_buffer = [0u16; 32];
        let mut length = 0;
        let mut dead_state = params.dead_state;
        unsafe {
            UCKeyTranslate(
                layout_data,
                params.virtual_key_code,
                if params.display {
                    kUCKeyActionDisplay
                } else {
                    kUCKeyActionDown
                },
                modifier_key_state,
                kbd_type,
                if params.ignore_dead_keys {
                    1 << kUCKeyTranslateNoDeadKeysBit
                } else {
                    0
                },
                &mut dead_state,
                unicode_buffer.len() as _,
                &mut length,
                unicode_buffer.as_mut_ptr(),
            )
        };

        let text = String::from_utf16(unsafe {
            std::slice::from_raw_parts(unicode_buffer.as_mut_ptr(), length as _)
        })?;

        Ok(TranslateResults { text, dead_state })
    }
}

impl Inner {
    fn enable_opengl(&mut self) -> anyhow::Result<Rc<glium::backend::Context>> {
        let view = self.view_id.as_ref().unwrap().load();
        let glium_context = GlContextPair::create(*view)?;

        self.gl_context_pair.replace(glium_context.clone());

        Ok(glium_context.context)
    }

    /// <https://stackoverflow.com/a/22677690>
    /// <https://stackoverflow.com/a/12548163>
    /// <https://stackoverflow.com/a/8263841>
    /// <https://developer.apple.com/documentation/coreservices/1390584-uckeytranslate?language=objc>
    fn translate_key_event(
        &mut self,
        virtual_key_code: u16,
        modifier_flags: NSEventModifierFlags,
    ) -> anyhow::Result<TranslateStatus> {
        let keyboard = Keyboard::new();

        let mods = key_modifiers(modifier_flags);

        let config = &self.config;

        let use_dead_keys = if !config.use_dead_keys {
            false
        } else if mods.contains(Modifiers::LEFT_ALT) {
            config.send_composed_key_when_left_alt_is_pressed
        } else if mods.contains(Modifiers::RIGHT_ALT) {
            config.send_composed_key_when_right_alt_is_pressed
        } else {
            true
        };

        if let Some(DeadKeyState { dead_state }) = self.dead_pending.take() {
            let result = keyboard.translate(TranslateParams {
                virtual_key_code,
                modifier_flags,
                dead_state,
                ignore_dead_keys: false,
                display: true,
            })?;

            // If length == 0 it means that they double-pressed the dead key.
            // We treat that the same as the dead key disabled state:
            // we want to clock through a space keypress so that we clear
            // the state and output the original keypress.
            let generate_space = !use_dead_keys || result.text.len() == 0;

            if generate_space {
                // synthesize a SPACE press to
                // elicit the underlying key code and get out
                // of the dead key state
                let result = keyboard.translate(TranslateParams {
                    virtual_key_code,
                    modifier_flags,
                    dead_state: result.dead_state,
                    ignore_dead_keys: false,
                    display: false,
                })?;
                Ok(TranslateStatus::Composed(result.text))
            } else {
                Ok(TranslateStatus::Composed(result.text))
            }
        } else if use_dead_keys {
            let result = keyboard.translate(TranslateParams {
                virtual_key_code,
                modifier_flags,
                dead_state: 0,
                ignore_dead_keys: false,
                display: false,
            })?;

            self.dead_pending.replace(DeadKeyState {
                dead_state: result.dead_state,
            });

            // Get the non-dead-key rendition to show as the composing state
            let composing = keyboard.translate(TranslateParams {
                virtual_key_code,
                modifier_flags,
                dead_state: 0,
                ignore_dead_keys: true,
                display: true,
            })?;

            Ok(TranslateStatus::Composing(composing.text))
        } else {
            Ok(TranslateStatus::NotDead)
        }
    }
}

const VIEW_CLS_NAME: &str = "WezTermWindowView";
const WINDOW_CLS_NAME: &str = "WezTermWindow";
const TITLEBAR_VIEW_NAME: &str = "NSTitlebarContainerView";

/// Name of the ivar on the view that stores the cursor we last asked for,
/// encoded via [`cursor_to_code`]. macOS aggressively resets the cursor to
/// its default (arrow) as part of its own cursor-management cycle; this lets
/// our `cursorUpdate:` handler re-assert the cursor we actually want.
const CURSOR_IVAR: &str = "thinktermCursorCode";

/// Returns the shared `NSCursor` instance for the given logical cursor.
unsafe fn ns_cursor_instance(cursor: MouseCursor) -> id {
    let cls = class!(NSCursor);
    match cursor {
        MouseCursor::Arrow => msg_send![cls, arrowCursor],
        MouseCursor::Text => msg_send![cls, IBeamCursor],
        MouseCursor::Hand => msg_send![cls, pointingHandCursor],
        MouseCursor::SizeUpDown => msg_send![cls, resizeUpDownCursor],
        MouseCursor::SizeLeftRight => msg_send![cls, resizeLeftRightCursor],
        // AppKit doesn't expose public diagonal resize cursor selectors.
        MouseCursor::SizeNorthWestSouthEast | MouseCursor::SizeNorthEastSouthWest => {
            msg_send![cls, arrowCursor]
        }
    }
}

/// Encode a cursor as a small integer suitable for storing in an ivar.
/// `0` means "no managed cursor" (eg: hidden while typing).
fn cursor_to_code(cursor: Option<MouseCursor>) -> i64 {
    match cursor {
        None => 0,
        Some(MouseCursor::Arrow) => 1,
        Some(MouseCursor::Text) => 2,
        Some(MouseCursor::Hand) => 3,
        Some(MouseCursor::SizeUpDown) => 4,
        Some(MouseCursor::SizeLeftRight) => 5,
        Some(MouseCursor::SizeNorthWestSouthEast) => 6,
        Some(MouseCursor::SizeNorthEastSouthWest) => 7,
    }
}

/// Inverse of [`cursor_to_code`].
fn code_to_cursor(code: i64) -> Option<MouseCursor> {
    match code {
        1 => Some(MouseCursor::Arrow),
        2 => Some(MouseCursor::Text),
        3 => Some(MouseCursor::Hand),
        4 => Some(MouseCursor::SizeUpDown),
        5 => Some(MouseCursor::SizeLeftRight),
        6 => Some(MouseCursor::SizeNorthWestSouthEast),
        7 => Some(MouseCursor::SizeNorthEastSouthWest),
        _ => None,
    }
}

struct WindowView {
    inner: Rc<RefCell<Inner>>,
    // Notifications must live outside `inner`: AppKit can run nested event
    // loops while an event handler holds that RefCell. Dropping work on a
    // failed try_borrow loses PaneOutput repaints and even the timer tick that
    // would otherwise recover them.
    pending_notifications: RefCell<PendingNotificationQueue<PendingWindowNotification>>,
    // Repaint scheduling flags live outside the RefCell: they are touched
    // from paths that can run while `inner` is borrowed (reentrant drawRect,
    // the frame throttle timer, invalidate()), and losing an update here is
    // what makes the window stop refreshing until the next user interaction.
    paint_throttled: Cell<bool>,
    // When the throttle was engaged. The async timer that clears the flag is
    // a single point of failure -- display sleep coalesces timers hard enough
    // to lose it outright, and once it is gone every drawRect short-circuits
    // on the flag forever (observed: a window frozen from the 50th minute of
    // a display-off stretch until relaunch). This timestamp is what lets
    // drawRect notice that the throttle has outlived any legal frame period
    // and break the latch itself.
    paint_throttled_since: Cell<Option<Instant>>,
    invalidated: Cell<bool>,
}

pub fn superclass(this: &Object) -> &'static Class {
    unsafe {
        let superclass: id = msg_send![this, superclass];
        &*(superclass as *const _)
    }
}

fn dpi_for_window_screen(ns_window: *mut Object, config: &ConfigHandle) -> Option<f64> {
    if config.dpi_by_screen.is_empty() {
        return config.dpi;
    }

    let screen = unsafe { msg_send![ns_window, screen] };
    let info = crate::os::macos::connection::nsscreen_to_screen_info(screen);

    config.dpi_by_screen.get(&info.name).copied()
}

#[allow(clippy::identity_op)]
fn decode_mouse_buttons(mask: u64) -> MouseButtons {
    let mut buttons = MouseButtons::NONE;

    if (mask & (1 << 0)) != 0 {
        buttons |= MouseButtons::LEFT;
    }
    if (mask & (1 << 1)) != 0 {
        buttons |= MouseButtons::RIGHT;
    }
    if (mask & (1 << 2)) != 0 {
        buttons |= MouseButtons::MIDDLE;
    }
    if (mask & (1 << 3)) != 0 {
        buttons |= MouseButtons::X1;
    }
    if (mask & (1 << 4)) != 0 {
        buttons |= MouseButtons::X2;
    }
    buttons
}

fn key_modifiers(flags: NSEventModifierFlags) -> Modifiers {
    let mut mods = Modifiers::NONE;

    if flags.contains(NSEventModifierFlags::NSShiftKeyMask) {
        mods |= Modifiers::SHIFT;
    }
    if flags.contains(NSEventModifierFlags::NSAlternateKeyMask) && (flags.bits() & 0x20) != 0 {
        mods |= Modifiers::LEFT_ALT | Modifiers::ALT;
    }
    if flags.contains(NSEventModifierFlags::NSAlternateKeyMask) && (flags.bits() & 0x40) != 0 {
        mods |= Modifiers::RIGHT_ALT | Modifiers::ALT;
    }
    if flags.contains(NSEventModifierFlags::NSControlKeyMask) {
        mods |= Modifiers::CTRL;
    }
    if flags.contains(NSEventModifierFlags::NSCommandKeyMask) {
        mods |= Modifiers::SUPER;
    }

    mods
}

/// We register our own subclass of NSWindow so that we can override
/// canBecomeKeyWindow so that our simple fullscreen style can keep
/// focus once the titlebar has been removed; the default behavior of
/// NSWindow is to reject focus when it doesn't have a titlebar!
fn get_window_class() -> &'static Class {
    Class::get(WINDOW_CLS_NAME).unwrap_or_else(|| {
        let mut cls = ClassDecl::new(WINDOW_CLS_NAME, class!(NSWindow))
            .expect("Unable to register Window class");

        extern "C" fn yes(_: &mut Object, _: Sel) -> BOOL {
            YES
        }

        unsafe {
            cls.add_method(
                sel!(canBecomeKeyWindow),
                yes as extern "C" fn(&mut Object, Sel) -> BOOL,
            );
            cls.add_method(
                sel!(canBecomeMainWindow),
                yes as extern "C" fn(&mut Object, Sel) -> BOOL,
            );
        }

        cls.register()
    })
}

impl WindowView {
    extern "C" fn dealloc(this: &mut Object, _sel: Sel) {
        Self::drop_inner(this);
        unsafe {
            let superclass = superclass(this);
            let () = msg_send![super(this, superclass), dealloc];
        }
    }

    fn drop_inner(this: &mut Object) {
        unsafe {
            let myself: *mut c_void = *this.get_ivar(VIEW_CLS_NAME);
            this.set_ivar(VIEW_CLS_NAME, std::ptr::null_mut() as *mut c_void);

            if !myself.is_null() {
                let myself = Box::from_raw(myself as *mut Self);
                drop(myself);
            }
        }
    }

    // Called by the inputContext manager when the IME processes events.
    // We need to translate the selector back into appropriate key
    // sequences
    extern "C" fn do_command_by_selector(this: &mut Object, _sel: Sel, a_selector: Sel) {
        let selector = format!("{:?}", a_selector);
        log::trace!("do_command_by_selector {:?}", selector);

        if let Some(myself) = Self::get_this(this) {
            let mut inner = myself.inner.borrow_mut();
            inner.ime_state = ImeDisposition::Continue;
            inner.ime_last_event.take();
        }
    }

    extern "C" fn has_marked_text(this: &mut Object, _sel: Sel) -> BOOL {
        if let Some(myself) = Self::get_this(this) {
            let inner = myself.inner.borrow();
            if inner.ime_text.is_empty() {
                NO
            } else {
                YES
            }
        } else {
            NO
        }
    }

    extern "C" fn marked_range(this: &mut Object, _sel: Sel) -> NSRange {
        if let Some(myself) = Self::get_this(this) {
            let inner = myself.inner.borrow();
            log::trace!("marked_range {:?}", inner.ime_text);
            if inner.ime_text.is_empty() {
                NSRange::new(NSNotFound as _, 0)
            } else {
                let start = inner
                    .native_text_input_snapshot
                    .as_ref()
                    .map(|snapshot| utf16_offset_for_byte(&snapshot.text, snapshot.selection.start))
                    .unwrap_or(0);
                NSRange::new(start as u64, inner.ime_text.encode_utf16().count() as u64)
            }
        } else {
            NSRange::new(NSNotFound as _, 0)
        }
    }

    extern "C" fn selected_range(this: &mut Object, _sel: Sel) -> NSRange {
        let Some(myself) = Self::get_this(this) else {
            return NSRange::new(NSNotFound as _, 0);
        };
        let inner = myself.inner.borrow();
        let Some(snapshot) = inner.native_text_input_snapshot.as_ref() else {
            // The terminal has no reified text storage, but system text
            // services still need a valid caret: macOS dictation queries
            // selectedRange and treats NSNotFound as "no editable text",
            // silently discarding the dictated words. Reporting an empty
            // selection at 0 lets dictation deliver text through the normal
            // insertText: path.
            return NSRange::new(0, 0);
        };
        let start = utf16_offset_for_byte(&snapshot.text, snapshot.selection.start);
        let end = utf16_offset_for_byte(&snapshot.text, snapshot.selection.end);
        NSRange::new(start as u64, end.saturating_sub(start) as u64)
    }

    // Called by the IME when inserting composed text and/or emoji
    extern "C" fn insert_text_replacement_range(
        this: &mut Object,
        _sel: Sel,
        astring: id,
        replacement_range: NSRange,
    ) {
        let s = unsafe { nsstring_to_str(astring) };
        log::trace!(
            "insert_text_replacement_range {} {:?}",
            s,
            replacement_range
        );
        if let Some(myself) = Self::get_this(this) {
            let mut inner = myself.inner.borrow_mut();

            if let Some(snapshot) = inner.native_text_input_snapshot.clone() {
                let relative = if replacement_range.0.location == NSNotFound as u64 {
                    snapshot.selection.clone()
                } else {
                    let start = byte_offset_for_utf16(
                        &snapshot.text,
                        replacement_range.0.location as usize,
                    );
                    let end = byte_offset_for_utf16(
                        &snapshot.text,
                        replacement_range
                            .0
                            .location
                            .saturating_add(replacement_range.0.length)
                            as usize,
                    );
                    start..end.max(start)
                };
                inner.ime_text.clear();
                inner
                    .events
                    .dispatch(WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None));
                inner.events.dispatch(WindowEvent::NativeTextInputReplace {
                    token: snapshot.token,
                    revision: snapshot.revision,
                    source_range: snapshot.source_base + relative.start
                        ..snapshot.source_base + relative.end,
                    text: s.to_string(),
                });
                inner.ime_last_event.take();
                inner.ime_state = ImeDisposition::Acted;
                return;
            }

            let key_is_down = inner.key_is_down.take().unwrap_or(true);

            let key = KeyCode::composed(s);

            let event = KeyEvent {
                key,
                modifiers: Modifiers::NONE,
                leds: KeyboardLedStatus::empty(),
                repeat_count: 1,
                key_is_down,
                raw: None,
            };

            inner.ime_text.clear();
            inner
                .events
                .dispatch(WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None));
            inner.ime_last_event.replace(event.clone());
            inner.events.dispatch(WindowEvent::KeyEvent(event));
            inner.ime_state = ImeDisposition::Acted;
        }
    }

    extern "C" fn set_marked_text_selected_range_replacement_range(
        this: &mut Object,
        _sel: Sel,
        astring: id,
        selected_range: NSRange,
        replacement_range: NSRange,
    ) {
        let s = unsafe { nsstring_to_str(astring) };
        log::trace!(
            "set_marked_text_selected_range_replacement_range {} {:?} {:?}",
            s,
            selected_range,
            replacement_range
        );
        if let Some(myself) = Self::get_this(this) {
            let mut inner = myself.inner.borrow_mut();
            inner.ime_text = s.to_string();

            // Advise the GUI right here rather than relying on the keyDown
            // handler's post-interpretKeyEvents dispatch: keyboard IMEs are
            // always driven by a key event, but dictation updates the marked
            // text spontaneously with no key event at all, and without this
            // the live transcription is invisible until the final insertText.
            // The keyDown path may advise the same status again; that is a
            // harmless duplicate.
            let status = if inner.ime_text.is_empty() {
                DeadKeyStatus::None
            } else {
                DeadKeyStatus::Composing(inner.ime_text.clone())
            };
            inner
                .events
                .dispatch(WindowEvent::AdviseDeadKeyStatus(status));

            inner.ime_last_event.take();
            inner.ime_state = ImeDisposition::Acted;
        }
    }

    extern "C" fn unmark_text(this: &mut Object, _sel: Sel) {
        log::trace!("unmarkText");
        if let Some(myself) = Self::get_this(this) {
            let mut inner = myself.inner.borrow_mut();
            // FIXME: docs say to insert the text here,
            // but iterm doesn't... and we've never seen
            // this get called so far?
            inner.ime_text.clear();
            inner
                .events
                .dispatch(WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None));
            inner.ime_last_event.take();
            inner.ime_state = ImeDisposition::Acted;
        }
    }

    extern "C" fn valid_attributes_for_marked_text(_this: &mut Object, _sel: Sel) -> id {
        // FIXME: returns NSArray<NSAttributedStringKey> *
        // log::trace!("valid_attributes_for_marked_text");
        // nil
        unsafe { NSArray::arrayWithObjects(nil, &[]) }
    }

    extern "C" fn attributed_substring_for_proposed_range(
        this: &mut Object,
        _sel: Sel,
        proposed_range: NSRange,
        actual_range: NSRangePointer,
    ) -> id {
        let Some(myself) = Self::get_this(this) else {
            return nil;
        };
        let inner = myself.inner.borrow();
        let Some(snapshot) = inner.native_text_input_snapshot.as_ref() else {
            return nil;
        };
        let total = snapshot.text.encode_utf16().count();
        let start_utf16 = (proposed_range.0.location as usize).min(total);
        let end_utf16 = start_utf16
            .saturating_add(proposed_range.0.length as usize)
            .min(total);
        let start = byte_offset_for_utf16(&snapshot.text, start_utf16);
        let end = byte_offset_for_utf16(&snapshot.text, end_utf16).max(start);
        if !actual_range.0.is_null() {
            unsafe {
                *actual_range.0 = NSRange::new(
                    start_utf16 as u64,
                    end_utf16.saturating_sub(start_utf16) as u64,
                );
            }
        }
        let substring = nsstring(&snapshot.text[start..end]);
        unsafe {
            let attributed: id = msg_send![class!(NSAttributedString), alloc];
            let attributed: id = msg_send![attributed, initWithString:*substring];
            let attributed: id = msg_send![attributed, autorelease];
            attributed
        }
    }

    extern "C" fn character_index_for_point(
        this: &mut Object,
        _sel: Sel,
        point: NSPoint,
    ) -> NSUInteger {
        let Some(myself) = Self::get_this(this) else {
            return NSNotFound as _;
        };
        let inner = myself.inner.borrow();
        let Some(snapshot) = inner.native_text_input_snapshot.as_ref() else {
            return NSNotFound as _;
        };
        let window: id = unsafe { msg_send![this, window] };
        let window_point: NSPoint = unsafe { msg_send![window, convertPointFromScreen:point] };
        let view_point: NSPoint =
            unsafe { msg_send![this, convertPoint:window_point fromView:nil] };
        let view_rect = NSRect::new(view_point, NSSize::new(1.0, 1.0));
        let backing: NSRect = unsafe { msg_send![this, convertRectToBacking:view_rect] };
        let hit = snapshot.hits.iter().min_by(|a, b| {
            let distance = |hit: &crate::NativeTextHit| {
                let cx = hit.rect.origin.x as f64 + hit.rect.size.width as f64 / 2.0;
                let cy = hit.rect.origin.y as f64 + hit.rect.size.height as f64 / 2.0;
                (cx - backing.origin.x).powi(2) + (cy - backing.origin.y).powi(2)
            };
            distance(a).total_cmp(&distance(b))
        });
        hit.map(|hit| utf16_offset_for_byte(&snapshot.text, hit.byte) as NSUInteger)
            .unwrap_or(NSNotFound as _)
    }

    extern "C" fn first_rect_for_character_range(
        this: &mut Object,
        _sel: Sel,
        range: NSRange,
        actual: NSRangePointer,
    ) -> NSRect {
        // Returns a rect in screen coordinates; this is used to place
        // the input method editor
        log::trace!(
            "firstRectForCharacterRange: range:{:?} actual:{:?}",
            range,
            actual
        );
        let window: id = unsafe { msg_send![this, window] };
        let frame = unsafe { NSWindow::frame(window) };
        let content: NSRect = unsafe { msg_send![window, contentRectForFrameRect: frame] };
        let backing_frame: NSRect = unsafe { msg_send![this, convertRectToBacking: frame] };
        let scale = frame.size.width / backing_frame.size.width;

        if let Some(this) = Self::get_this(this) {
            let inner = this.inner.borrow();
            let target = inner
                .native_text_input_snapshot
                .as_ref()
                .and_then(|snapshot| {
                    let byte = byte_offset_for_utf16(&snapshot.text, range.0.location as usize);
                    snapshot
                        .hits
                        .iter()
                        .min_by_key(|hit| hit.byte.abs_diff(byte))
                        .map(|hit| hit.rect)
                });
            let cursor_pos = target
                .unwrap_or(inner.text_cursor_position)
                .to_f64()
                .scale(scale, scale);

            if !actual.0.is_null() {
                unsafe {
                    *actual.0 = range;
                }
            }

            NSRect::new(
                NSPoint::new(
                    content.origin.x + cursor_pos.min_x(),
                    content.origin.y + content.size.height - cursor_pos.max_y(),
                ),
                NSSize::new(cursor_pos.size.width, cursor_pos.size.height),
            )
        } else {
            frame
        }
    }

    extern "C" fn accepts_first_mouse(_this: &mut Object, _sel: Sel, _nsevent: id) -> BOOL {
        YES
    }

    extern "C" fn accepts_first_responder(_this: &mut Object, _sel: Sel) -> BOOL {
        YES
    }

    extern "C" fn view_did_change_effective_appearance(this: &mut Object, _sel: Sel) {
        if let Some(this) = Self::get_this(this) {
            let Some(connection) = Connection::get() else {
                return;
            };
            let appearance = connection.get_appearance();
            if let Ok(mut inner) = this.inner.try_borrow_mut() {
                inner
                    .events
                    .dispatch(WindowEvent::AppearanceChanged(appearance));
            } else {
                log::trace!(
                    "deferring appearance change because the macOS window is dispatching another event"
                );
            }
        }
    }

    extern "C" fn update_tracking_areas(this: &mut Object, _sel: Sel) {
        let frame = unsafe { NSView::frame(this as *mut _) };

        if let Some(this) = Self::get_this(this) {
            let mut inner = this.inner.borrow_mut();
            if let Some(ref view) = inner.view_id {
                let view = view.load();
                if view.is_null() {
                    return;
                }

                let tag = inner.tracking_rect_tag;
                let size = (frame.size.width, frame.size.height);

                // AppKit calls `updateTrackingAreas` for all sorts of reasons
                // beyond an actual geometry change. Tearing the rect down and
                // re-adding it is not free: `removeTrackingRect` while the
                // pointer is inside emits a `mouseExited`, and the replacement
                // is registered with `assumeInside: NO`, so AppKit then owes us
                // a fresh `mouseEntered`. That exit/enter pair clears
                // `current_mouse_event` on the GUI side and makes hover
                // highlights flicker between hovered and idle. Rebuild only
                // when the tracked area really changed.
                if tag != 0 && inner.tracking_rect_size == size {
                    return;
                }

                if tag != 0 {
                    unsafe {
                        let () = msg_send![*view, removeTrackingRect: tag];
                    }
                }

                let rect = NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(frame.size.width, frame.size.height),
                );
                inner.tracking_rect_tag = unsafe {
                    msg_send![*view, addTrackingRect: rect owner: *view userData: nil assumeInside: NO]
                };
                inner.tracking_rect_size = size;
            }
        }
    }

    extern "C" fn window_should_close(this: &mut Object, _sel: Sel, _id: id) -> BOOL {
        unsafe {
            let () = msg_send![this, setNeedsDisplay: YES];
        }

        if let Some(this) = Self::get_this(this) {
            this.inner
                .borrow_mut()
                .events
                .dispatch(WindowEvent::CloseRequested);
            NO
        } else {
            YES
        }
    }

    /// Ensure that the menubar is shown when we transition from a fullscreen window
    /// to either a non-fullscreen window or no windows.
    /// Without this, we can end up in a state where the menu bar is invisible when
    /// it should otherwise be visible, and it is especially confusing when there
    /// are no windows.
    fn update_application_presentation(&self, is_key: bool) {
        let is_simple_full_screen;
        let native_full_screen;

        {
            let Ok(inner) = self.inner.try_borrow() else {
                log::trace!("skipping application presentation update while window is busy");
                return;
            };
            native_full_screen = inner.config.native_macos_fullscreen_mode;
            is_simple_full_screen = inner.fullscreen.is_some();
        }

        if !native_full_screen {
            let current_app = unsafe { NSApplication::sharedApplication(nil) };
            let target_options = match (is_key, is_simple_full_screen) {
                (true, true) => {
                    NSApplicationPresentationOptions::NSApplicationPresentationAutoHideMenuBar
                        | NSApplicationPresentationOptions::NSApplicationPresentationAutoHideDock
                }
                (true, false) | (false, _) => {
                    NSApplicationPresentationOptions::NSApplicationPresentationDefault
                }
            };
            unsafe {
                let current_options: NSApplicationPresentationOptions =
                    msg_send![current_app, presentationOptions];
                if current_options != target_options {
                    current_app.setPresentationOptions_(target_options);
                }
            }
        }
    }

    extern "C" fn did_become_key(this: &mut Object, _sel: Sel, _id: id) {
        if let Some(this) = Self::get_this(this) {
            if let Ok(mut inner) = this.inner.try_borrow_mut() {
                inner.events.dispatch(WindowEvent::FocusChanged(true));
            } else {
                log::trace!("skipping focus gained notification while window is busy");
            }
            this.update_application_presentation(true);
        }
    }

    extern "C" fn did_resign_key(this: &mut Object, _sel: Sel, _id: id) {
        if let Some(this) = Self::get_this(this) {
            if let Ok(mut inner) = this.inner.try_borrow_mut() {
                inner.events.dispatch(WindowEvent::FocusChanged(false));
            } else {
                log::trace!("skipping focus lost notification while window is busy");
            }
            this.update_application_presentation(true);
        }
    }

    extern "C" fn did_change_occlusion_state(view: &mut Object, _sel: Sel, _id: id) {
        // AppKit suppresses drawRect for occluded windows (covered, or on
        // another Space). If a frame was requested while we were hidden, the
        // needsDisplay it set may have been consumed without a draw; re-arm
        // when we become visible again so the window doesn't stay stale
        // until the next input event.
        const NS_WINDOW_OCCLUSION_STATE_VISIBLE: NSUInteger = 1 << 1;
        let view_ptr: id = view as *mut Object;
        if let Some(this) = Self::get_this(view) {
            let visible = unsafe {
                let window: id = msg_send![view_ptr, window];
                if window.is_null() {
                    false
                } else {
                    let state: NSUInteger = msg_send![window, occlusionState];
                    (state & NS_WINDOW_OCCLUSION_STATE_VISIBLE) != 0
                }
            };
            if visible && this.invalidated.get() {
                unsafe {
                    let () = msg_send![view_ptr, setNeedsDisplay: YES];
                }
            }
        }
    }

    // Switch the coordinate system to have 0,0 in the top left
    extern "C" fn is_flipped(_this: &Object, _sel: Sel) -> BOOL {
        YES
    }

    // Tell the window/view/layer stuff that we only have a single opaque
    // thing in the window so that it can optimize rendering
    extern "C" fn is_opaque(_this: &Object, _sel: Sel) -> BOOL {
        NO
    }

    // Don't use Cocoa native window tabbing
    extern "C" fn allow_automatic_tabbing(_this: &Object, _sel: Sel) -> BOOL {
        NO
    }

    extern "C" fn wezterm_perform_key_assignment(
        this: &mut Object,
        _sel: Sel,
        menu_item: *mut Object,
    ) {
        let menu_item = MenuItem::with_menu_item(menu_item);
        // Safe because weztermPerformKeyAssignment: is only used with KeyAssignment
        let action = menu_item.get_represented_item();
        log::debug!("wezterm_perform_key_assignment {action:?}",);
        match action {
            Some(RepresentedItem::KeyAssignment(action)) => {
                if let Some(this) = Self::get_this(this) {
                    this.inner
                        .borrow_mut()
                        .events
                        .dispatch(WindowEvent::PerformKeyAssignment(action));
                }
            }
            Some(RepresentedItem::ContextMenuAction(action)) => {
                if let Some(this) = Self::get_this(this) {
                    let event = match action {
                        crate::ContextMenuAction::KeyAssignment(action) => {
                            WindowEvent::PerformKeyAssignment(action)
                        }
                        crate::ContextMenuAction::ApplicationAction(action_id) => {
                            WindowEvent::PerformContextMenuAction(action_id)
                        }
                    };
                    this.inner.borrow_mut().events.dispatch(event);
                }
            }
            None => {}
        }
    }

    extern "C" fn thinkterm_toggle_workspace_sidebar(this: &mut Object, _sel: Sel, _sender: id) {
        if let Some(this) = Self::get_this(this) {
            this.inner
                .borrow_mut()
                .events
                .dispatch(WindowEvent::ToggleWorkspaceSidebar);
        }
    }

    extern "C" fn window_will_close(this: &mut Object, _sel: Sel, _id: id) {
        if let Some(this) = Self::get_this(this) {
            // Advise the window of its impending death
            this.inner
                .borrow_mut()
                .events
                .dispatch(WindowEvent::Destroyed);
            this.update_application_presentation(false);
            let conn = Connection::get().unwrap();
            let window_id = this.inner.borrow_mut().window_id;
            conn.windows.borrow_mut().remove(&window_id);
        }
    }

    fn mouse_common(
        this: &mut Object,
        nsevent: id,
        kind: MouseEventKind,
        precise_scroll_delta: Option<PreciseScrollDelta>,
        scroll_phase: Option<ScrollPhase>,
        momentum_phase: Option<ScrollPhase>,
    ) {
        let view = this as id;
        let coords;
        let mouse_buttons;
        let modifiers;
        let screen_coords;
        unsafe {
            let point = NSView::convertPoint_fromView_(view, nsevent.locationInWindow(), nil);
            let rect = NSRect::new(NSPoint::new(0., 0.), NSSize::new(point.x, point.y));
            let backing_rect = NSView::convertRectToBacking(view, rect);
            // backing_rect computes abs() values, so we need to restore the sign
            // from the original point
            coords = NSPoint::new(
                f64::copysign(backing_rect.size.width, point.x),
                f64::copysign(backing_rect.size.height, point.y),
            );
            mouse_buttons = decode_mouse_buttons(NSEvent::pressedMouseButtons(nsevent));
            modifiers = key_modifiers(nsevent.modifierFlags());
            screen_coords = NSEvent::mouseLocation(nsevent);
        }
        let event = MouseEvent {
            kind,
            coords: Point::new(coords.x as isize, coords.y as isize),
            screen_coords: cartesian_to_screen_point(screen_coords),
            mouse_buttons,
            modifiers,
            precise_scroll_delta,
            scroll_phase,
            momentum_phase,
        };

        if let Some(myself) = Self::get_this(this) {
            let mut inner = myself.inner.borrow_mut();
            inner.events.dispatch(WindowEvent::MouseEvent(event));
        }
    }

    extern "C" fn mouse_up(this: &mut Object, _sel: Sel, nsevent: id) {
        Self::mouse_common(
            this,
            nsevent,
            MouseEventKind::Release(MousePress::Left),
            None,
            None,
            None,
        );
    }

    extern "C" fn mouse_down(this: &mut Object, _sel: Sel, nsevent: id) {
        Self::mouse_common(
            this,
            nsevent,
            MouseEventKind::Press(MousePress::Left),
            None,
            None,
            None,
        );
    }
    extern "C" fn right_mouse_up(this: &mut Object, _sel: Sel, nsevent: id) {
        Self::mouse_common(
            this,
            nsevent,
            MouseEventKind::Release(MousePress::Right),
            None,
            None,
            None,
        );
    }

    extern "C" fn other_mouse_up(this: &mut Object, _sel: Sel, nsevent: id) {
        // Safety: We know this is an button event
        unsafe {
            let button_number = NSEvent::buttonNumber(nsevent);
            // Button 2 is the middle mouse button (scroll wheel)
            // but is the dedicated middle mouse button on 4 button mouses
            if button_number == 2 {
                Self::mouse_common(
                    this,
                    nsevent,
                    MouseEventKind::Release(MousePress::Middle),
                    None,
                    None,
                    None,
                );
            }
        }
    }

    extern "C" fn scroll_wheel(this: &mut Object, _sel: Sel, nsevent: id) {
        let precise = unsafe { nsevent.hasPreciseScrollingDeltas() } == YES;
        let raw_vert_delta = unsafe { nsevent.scrollingDeltaY() };
        let raw_horz_delta = unsafe { nsevent.scrollingDeltaX() };
        let scroll_phase = unsafe { ns_event_phase_to_scroll_phase(nsevent.phase()) };
        let momentum_phase = unsafe { ns_event_phase_to_scroll_phase(nsevent.momentumPhase()) };
        let precise_scroll_delta = if precise
            && (raw_vert_delta.abs() > f64::EPSILON || raw_horz_delta.abs() > f64::EPSILON)
        {
            // scrollingDeltaX/Y are in points; mouse coordinates and all of
            // the chrome geometry consuming this payload are in backing
            // pixels (see the convertRectToBacking in mouse_common). Convert
            // here so every consumer receives uniform physical pixels.
            let rect = NSRect::new(
                NSPoint::new(0., 0.),
                NSSize::new(raw_horz_delta, raw_vert_delta),
            );
            let backing = unsafe { NSView::convertRectToBacking(this as id, rect) };
            Some(PreciseScrollDelta {
                x: f64::copysign(backing.size.width, raw_horz_delta) as f32,
                y: f64::copysign(backing.size.height, raw_vert_delta) as f32,
            })
        } else {
            None
        };
        let scale = if precise {
            // Devices with precise deltas report number of pixels scrolled.
            // At this layer we don't know how many pixels comprise a cell
            // in the terminal widget, and our abstraction doesn't allow being
            // told what that amount should be, so we come up with a hard
            // coded factor based on the likely default font size and dpi
            // to make the scroll speed feel a bit better.
            15.0
        } else {
            // Whereas imprecise deltas report the number of lines scrolled,
            // so we want to report those lines here wholesale.
            1.0
        };
        let mut vert_delta = raw_vert_delta / scale;
        let mut horz_delta = raw_horz_delta / scale;

        if let Some(myself) = Self::get_this(this) {
            let mut inner = myself.inner.borrow_mut();

            let elapsed = inner.last_wheel.elapsed();

            // If it's been a while since the last wheel movement,
            // we want to clear out any accumulated fractional amount
            // and round this event up to 1 line so that we get an
            // immediate scroll on the first move.
            let stale = std::time::Duration::from_millis(250);
            if elapsed >= stale {
                if vert_delta != 0.0 && vert_delta.abs() < 1.0 {
                    vert_delta = round_away_from_zerof(vert_delta);
                }
                if horz_delta != 0.0 && horz_delta.abs() < 1.0 {
                    horz_delta = round_away_from_zerof(horz_delta);
                }
                inner.vscroll_remainder = 0.;
                inner.hscroll_remainder = 0.;
            }

            inner.last_wheel = Instant::now();

            // Reset remainder when changing scroll direction
            if vert_delta.signum() != inner.vscroll_remainder.signum() {
                inner.vscroll_remainder = 0.;
            }
            if horz_delta.signum() != inner.hscroll_remainder.signum() {
                inner.hscroll_remainder = 0.;
            }

            vert_delta += inner.vscroll_remainder;
            horz_delta += inner.hscroll_remainder;

            inner.vscroll_remainder = vert_delta.fract();
            inner.hscroll_remainder = horz_delta.fract();

            vert_delta = vert_delta.trunc();
            horz_delta = horz_delta.trunc();
        }

        // Precise devices report pixel deltas. Always dispatch those events,
        // even when the legacy line accumulator has not reached a whole line;
        // pixel-scrolling surfaces (Files/Note/sidebars) consume the precise
        // payload directly while terminal grids keep using the integer kind.
        if !should_dispatch_scroll_event(
            vert_delta,
            horz_delta,
            precise_scroll_delta.is_some(),
            scroll_phase,
            momentum_phase,
        ) {
            return;
        }

        let vertical_is_dominant = if vert_delta.abs() >= 1.0 || horz_delta.abs() >= 1.0 {
            vert_delta.abs() > horz_delta.abs()
        } else {
            raw_vert_delta.abs() > raw_horz_delta.abs()
        };
        let kind = if vertical_is_dominant {
            MouseEventKind::VertWheel(if vert_delta.abs() >= 1.0 {
                round_away_from_zero(vert_delta)
            } else {
                0
            })
        } else {
            MouseEventKind::HorzWheel(if horz_delta.abs() >= 1.0 {
                round_away_from_zero(horz_delta)
            } else {
                0
            })
        };
        if thinkterm_perf_enabled() {
            log::info!(
                "thinkterm_perf scroll precise={precise} raw=({raw_horz_delta:.2},{raw_vert_delta:.2}) phase={scroll_phase:?} momentum={momentum_phase:?}"
            );
        }
        Self::mouse_common(
            this,
            nsevent,
            kind,
            precise_scroll_delta,
            scroll_phase,
            momentum_phase,
        );
    }

    /// Opting in here is what makes AppKit deliver a horizontal page swipe as
    /// phased scroll events, which `scroll_wheel` above turns into the
    /// interactive sidebar gesture.
    ///
    /// Deliberately not paired with a `swipeWithEvent:` fallback. That
    /// responder callback fires window-wide with no notion of what is under
    /// the pointer, so synthesizing a wheel event from it injects
    /// WheelLeft/WheelRight into whatever owns that spot -- a terminal running
    /// an alt-screen application, most of the time. It also carries a single
    /// discrete delta rather than a stream, so the gesture it produces can only
    /// ever cut straight to the destination.
    extern "C" fn wants_scroll_events_for_swipe_tracking(
        _this: &Object,
        _sel: Sel,
        axis: NSInteger,
    ) -> BOOL {
        if axis == appkit::NSEventGestureAxis::NSEventGestureAxisHorizontal as NSInteger {
            YES
        } else {
            NO
        }
    }

    extern "C" fn right_mouse_down(this: &mut Object, _sel: Sel, nsevent: id) {
        Self::mouse_common(
            this,
            nsevent,
            MouseEventKind::Press(MousePress::Right),
            None,
            None,
            None,
        );
    }

    extern "C" fn other_mouse_down(this: &mut Object, _sel: Sel, nsevent: id) {
        // Safety: See `other_mouse_up`
        unsafe {
            let button_number = NSEvent::buttonNumber(nsevent);
            // See `other_mouse_up`
            if button_number == 2 {
                Self::mouse_common(
                    this,
                    nsevent,
                    MouseEventKind::Press(MousePress::Middle),
                    None,
                    None,
                    None,
                );
            }
        }
    }

    extern "C" fn mouse_moved_or_dragged(this: &mut Object, _sel: Sel, nsevent: id) {
        Self::mouse_common(this, nsevent, MouseEventKind::Move, None, None, None);
    }

    extern "C" fn mouse_exited(this: &mut Object, _sel: Sel, nsevent: id) {
        if let Some(myself) = Self::get_this(this) {
            let event = if event_is_sidebar_button_tracking(nsevent) {
                WindowEvent::WorkspaceSidebarButtonHover(false)
            } else {
                WindowEvent::MouseLeave
            };
            myself.inner.borrow_mut().events.dispatch(event);
        }
    }

    /// Only the titlebar sidebar button's tracking area is interesting here;
    /// the content view's own area announces itself through mouseMoved.
    extern "C" fn mouse_entered(this: &mut Object, _sel: Sel, nsevent: id) {
        if !event_is_sidebar_button_tracking(nsevent) {
            return;
        }
        if let Some(myself) = Self::get_this(this) {
            myself
                .inner
                .borrow_mut()
                .events
                .dispatch(WindowEvent::WorkspaceSidebarButtonHover(true));
        }
    }

    /// AppKit calls this whenever it (re)builds the view's cursor rectangles,
    /// which is also the moment it would otherwise reset the cursor to the
    /// default arrow. By registering a cursor rect for the cursor we last
    /// requested, we let AppKit's own cursor management paint our cursor over
    /// the view instead of the arrow. This is what stops the I-beam from
    /// flickering to an arrow while the main thread is busy painting heavy
    /// terminal output (our `mouseMoved:`-driven `[NSCursor set]` calls get
    /// coalesced/delayed, so relying on them alone is not enough).
    extern "C" fn reset_cursor_rects(this: &mut Object, _sel: Sel) {
        unsafe {
            let code: i64 = *this.get_ivar::<i64>(CURSOR_IVAR);
            if let Some(cursor) = code_to_cursor(code) {
                let instance = ns_cursor_instance(cursor);
                let bounds: NSRect = msg_send![this, bounds];
                let () = msg_send![this, addCursorRect: bounds cursor: instance];
            }
            // code == 0 means we have no managed cursor (eg: hidden while
            // typing); leave AppKit's default behaviour in that case.
        }
    }

    fn key_common(this: &mut Object, nsevent: id, key_is_down: bool) {
        let is_a_repeat = unsafe { nsevent.isARepeat() == YES };
        let chars = unsafe { nsstring_to_str(nsevent.characters()) };
        let unmod = unsafe { nsstring_to_str(nsevent.charactersIgnoringModifiers()) };
        let modifier_flags = unsafe { nsevent.modifierFlags() };
        let modifiers = key_modifiers(modifier_flags);
        let leds = if modifier_flags.bits() & (1 << 16) != 0 {
            KeyboardLedStatus::CAPS_LOCK
        } else {
            KeyboardLedStatus::empty()
        };
        let virtual_key = unsafe { nsevent.keyCode() };

        log::debug!(
            "key_common: chars=`{}` unmod=`{}` modifiers=`{:?}` virtual_key={:?} key_is_down:{}",
            chars.escape_debug(),
            unmod.escape_debug(),
            modifiers,
            virtual_key,
            key_is_down
        );

        // `Delete` on macos is really Backspace and emits BS.
        // `Fn-Delete` emits DEL.
        // Alt-Delete is mapped by the IME to be equivalent to Fn-Delete.
        // We want to emit Alt-BS in that situation.
        let (prefer_vkey, unmod) =
            if virtual_key == kVK_Delete && modifiers.contains(Modifiers::ALT) {
                (true, "\x08")
            } else if virtual_key == kVK_Tab {
                (true, "\t")
            } else if virtual_key == kVK_Delete {
                (true, "\x08")
            } else if virtual_key == kVK_ANSI_KeypadEnter {
                // https://github.com/wezterm/wezterm/issues/739
                // Keypad enter sends ctrl-c for some reason; explicitly
                // treat that as enter here.
                (true, "\r")
            } else {
                (false, unmod)
            };

        // Shift-Tab on macOS produces \x19 for some reason.
        // Rewrite it to something we understand.
        // <https://github.com/wezterm/wezterm/issues/1902>
        let chars = if virtual_key == kVK_Tab && modifiers.contains(Modifiers::SHIFT) {
            "\t"
        } else {
            chars
        };

        let phys_code = vkey_to_phys(virtual_key);
        let raw_key_handled = Handled::new();
        let raw_key_event = RawKeyEvent {
            key: if unmod.is_empty() {
                match phys_code {
                    Some(phys) => KeyCode::Physical(phys),
                    None => KeyCode::RawCode(virtual_key as _),
                }
            } else {
                KeyCode::composed(unmod)
            },
            phys_code,
            raw_code: virtual_key as _,
            leds,
            modifiers,
            repeat_count: 1,
            key_is_down,
            handled: raw_key_handled.clone(),
        };
        if let Some(myself) = Self::get_this(this) {
            let mut inner = myself.inner.borrow_mut();
            inner
                .events
                .dispatch(WindowEvent::RawKeyEvent(raw_key_event.clone()));
        }

        if raw_key_handled.is_handled() {
            log::trace!("raw key was handled; not processing further");
            return;
        }

        let chars = if let Some(myself) = Self::get_this(this) {
            let mut inner = myself.inner.borrow_mut();

            if chars.is_empty() || inner.dead_pending.is_some() {
                // Dead key!
                if !key_is_down {
                    return;
                }

                match inner.translate_key_event(virtual_key, modifier_flags) {
                    Ok(TranslateStatus::Composing(composing)) => {
                        // Next key press in dead key sequence is pending.
                        inner.events.dispatch(WindowEvent::AdviseDeadKeyStatus(
                            DeadKeyStatus::Composing(composing),
                        ));

                        return;
                    }
                    Ok(TranslateStatus::Composed(translated)) => {
                        inner
                            .events
                            .dispatch(WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None));
                        let event = KeyEvent {
                            key: KeyCode::composed(&translated),
                            modifiers: Modifiers::NONE,
                            leds: KeyboardLedStatus::empty(),
                            repeat_count: 1,
                            key_is_down,
                            raw: None,
                        };
                        inner.events.dispatch(WindowEvent::KeyEvent(event));
                        return;
                    }
                    Ok(TranslateStatus::NotDead) => {
                        // Turned out that while it would have been a dead
                        // key combo, our send_composed_key_when_XXX settings
                        // said otherwise. Let's continue as if it was not
                        // a dead key.
                        unmod
                    }
                    Err(e) => {
                        log::error!("Failed to translate dead key: {}", e);
                        return;
                    }
                }
            } else {
                chars
            }
        } else {
            return;
        };

        let config_handle = config::configuration();
        let use_ime = config_handle.use_ime;
        let send_composed_key_when_left_alt_is_pressed =
            config_handle.send_composed_key_when_left_alt_is_pressed;
        let send_composed_key_when_right_alt_is_pressed =
            config_handle.send_composed_key_when_right_alt_is_pressed;

        // If unmod is empty it most likely means that the user has selected
        // an alternate keymap that has a chorded representation of eg: an ASCII
        // character.  One example of this is selecting a Norwegian keymap on
        // a US keyboard.  The `~` symbol is produced by pressing CTRL-].
        // That shows up here as unmod=`` with modifiers=CTRL.  In this situation
        // we want to cancel the modifiers out so that we just focus on
        // `chars` instead.
        let modifiers = if unmod.is_empty() {
            Modifiers::NONE
        } else {
            modifiers
        };

        let alt_mods = Modifiers::LEFT_ALT | Modifiers::RIGHT_ALT | Modifiers::ALT;
        let only_left_alt = (modifiers & alt_mods) == (Modifiers::LEFT_ALT | Modifiers::ALT);
        let only_right_alt = (modifiers & alt_mods) == (Modifiers::RIGHT_ALT | Modifiers::ALT);

        // Also respect `send_composed_key_when_(left|right)_alt_is_pressed` configs
        // when `use_ime` is true.
        let forward_to_ime = {
            if only_left_alt && !send_composed_key_when_left_alt_is_pressed {
                false
            } else if only_right_alt && !send_composed_key_when_right_alt_is_pressed {
                false
            } else {
                modifiers.is_empty()
                    || modifiers.intersects(config_handle.macos_forward_to_ime_modifier_mask)
            }
        };

        if key_is_down && use_ime && forward_to_ime {
            if let Some(myself) = Self::get_this(this) {
                let mut inner = myself.inner.borrow_mut();
                inner.key_is_down.replace(key_is_down);
                inner.ime_state = ImeDisposition::None;
                inner.ime_text.clear();
            }

            unsafe {
                let array: id = msg_send![class!(NSArray), arrayWithObject: nsevent];
                let _: () = msg_send![this, interpretKeyEvents: array];

                if let Some(myself) = Self::get_this(this) {
                    let mut inner = myself.inner.borrow_mut();
                    log::trace!(
                        "IME state: {:?}, last_event: {:?}",
                        inner.ime_state,
                        inner.ime_last_event
                    );
                    match inner.ime_state {
                        ImeDisposition::Continue => {
                            // IME handled the event by generating NOOP;
                            // let's continue with our normal handling
                            // code below.
                            inner.ime_last_event.take();
                        }
                        ImeDisposition::Acted => {
                            // The key caused the IME to call one of our
                            // callbacks, which may have generated an event and
                            // stashed it into ime_last_event.
                            // If it didn't generate an event, then a composition
                            // is pending.
                            let status = if inner.ime_last_event.is_none() {
                                DeadKeyStatus::Composing(inner.ime_text.clone())
                            } else {
                                DeadKeyStatus::None
                            };
                            inner
                                .events
                                .dispatch(WindowEvent::AdviseDeadKeyStatus(status));
                            return;
                        }
                        ImeDisposition::None => {
                            // The IME clocked something in its state,
                            // but didn't call one of our callbacks.
                            // In theory, we should stop here, but the IME
                            // mysteriously swallows key repeats for certain
                            // keys (i.e. b, f, j, m, p, q, v, x) but not others.
                            // To compensate for that, if the current event
                            // is a repeat, and the IME previously generated
                            // `Acted`, we will assume that we're safe to replay
                            // that last action.
                            if is_a_repeat {
                                if let Some(event) =
                                    inner.ime_last_event.as_ref().map(|e| e.clone())
                                {
                                    inner.events.dispatch(WindowEvent::KeyEvent(event));
                                    return;
                                }
                            }
                            let status = if inner.ime_text.is_empty() {
                                DeadKeyStatus::None
                            } else {
                                DeadKeyStatus::Composing(inner.ime_text.clone())
                            };
                            inner
                                .events
                                .dispatch(WindowEvent::AdviseDeadKeyStatus(status));
                            return;
                        }
                    }
                }
            }
        }

        fn key_string_to_key_code(s: &str) -> Option<KeyCode> {
            let mut char_iter = s.chars();
            if let Some(first_char) = char_iter.next() {
                if char_iter.next().is_none() {
                    // A single unicode char
                    Some(function_key_to_keycode(first_char))
                } else {
                    Some(KeyCode::Composed(s.to_owned()))
                }
            } else {
                None
            }
        }

        // When both shift and alt are pressed, macos appears to swap `chars` with `unmod`,
        // which isn't particularly helpful. eg: ALT+SHIFT+` produces chars='`' and unmod='~'
        // In this case, we take the key from unmod.
        // We leave `raw` set to None as we want to preserve the value of modifiers.
        // <https://github.com/wezterm/wezterm/issues/1706>.
        // We can't do this for every ALT+SHIFT combo, as the weird behavior doesn't
        // apply to eg: ALT+SHIFT+789 for Norwegian layouts
        // <https://github.com/wezterm/wezterm/issues/760>
        let swap_unmod_and_chars = (modifiers.contains(Modifiers::SHIFT | Modifiers::ALT)
            && virtual_key == kVK_ANSI_Grave)
            ||
            // <https://github.com/wezterm/wezterm/issues/1907>
            (modifiers.contains(Modifiers::SHIFT | Modifiers::CTRL)
                && virtual_key == kVK_ANSI_Slash);

        if let Some(key) = key_string_to_key_code(chars).or_else(|| key_string_to_key_code(unmod)) {
            let (key, raw_key) = if prefer_vkey {
                match phys_code {
                    Some(phys) => (phys.to_key_code(), None),
                    None => {
                        log::error!(
                            "prefer_vkey=true, but phys_code is None. {:?}",
                            raw_key_event
                        );
                        return;
                    }
                }
            } else if (only_left_alt && !send_composed_key_when_left_alt_is_pressed)
                || (only_right_alt && !send_composed_key_when_right_alt_is_pressed)
            {
                // Take the unmodified key only!
                match key_string_to_key_code(unmod) {
                    Some(key) => (key, None),
                    None => return,
                }
            } else if chars.is_empty() || chars == unmod {
                (key, None)
            } else if swap_unmod_and_chars {
                match key_string_to_key_code(unmod) {
                    Some(key) => (key, None),
                    None => return,
                }
            } else {
                let raw = key_string_to_key_code(unmod);
                match (&key, &raw) {
                    // Avoid eg: \x01 when we can use CTRL-A.
                    // This also helps to keep the correct sequence for backspace/delete.
                    // But take care: on German layouts CTRL-Backslash has unmod="/"
                    // but chars="\x1c"; we only want to do this transformation when
                    // chars and unmod have that base ASCII relationship.
                    // <https://github.com/wezterm/wezterm/issues/1891>
                    (KeyCode::Char(c), Some(KeyCode::Char(raw)))
                        if is_ascii_control(*c) == Some(raw.to_ascii_lowercase()) =>
                    {
                        (KeyCode::Char(*raw), None)
                    }
                    _ => (key, raw),
                }
            };

            let modifiers = if raw_key.is_some() {
                Modifiers::NONE
            } else {
                modifiers
            };

            let event = KeyEvent {
                key,
                modifiers,
                leds,
                repeat_count: 1,
                key_is_down,
                raw: Some(raw_key_event),
            }
            .normalize_shift()
            .resurface_positional_modifier_key();

            log::debug!(
                "key_common {:?} (chars={:?} unmod={:?} modifiers={:?})",
                event,
                chars,
                unmod,
                modifiers
            );

            if let Some(myself) = Self::get_this(this) {
                let mut inner = myself.inner.borrow_mut();
                // Don't clear the last IME event when a key is up otherwise it
                // could mess up the succeeding key repeats.
                if key_is_down {
                    inner.ime_last_event.take();
                }
                inner.events.dispatch(WindowEvent::KeyEvent(event));
            }
        }
    }

    extern "C" fn perform_key_equivalent(this: &mut Object, _sel: Sel, nsevent: id) -> BOOL {
        let chars = unsafe { nsstring_to_str(nsevent.characters()) };
        let modifier_flags = unsafe { nsevent.modifierFlags() };
        let modifiers = key_modifiers(modifier_flags);

        log::trace!(
            "perform_key_equivalent: chars=`{}` modifiers=`{:?}`",
            chars.escape_debug(),
            modifiers,
        );

        if (chars == "." && modifiers == Modifiers::SUPER)
            || (chars == "\u{1b}" && modifiers == Modifiers::CTRL)
            || (chars == "\t" && modifiers == Modifiers::CTRL)
            || (chars == "\x19"/* Shift-Tab: See issue #1902 */)
        {
            // Synthesize a key down event for this, because macOS will
            // not do that, even though we tell it that we handled this event.
            // <https://github.com/wezterm/wezterm/issues/1867>
            Self::key_common(this, nsevent, true);

            // Prevent macOS from calling doCommandBySelector(cancel:)
            YES
        } else {
            // Allow macOS to process built-in shortcuts like CMD-`
            // to cycle though windows
            NO
        }
    }

    extern "C" fn flags_changed(this: &mut Object, _sel: Sel, nsevent: id) {
        let modifier_flags = unsafe { nsevent.modifierFlags() };
        let modifiers = key_modifiers(modifier_flags);
        let leds = if modifier_flags.bits() & (1 << 16) != 0 {
            KeyboardLedStatus::CAPS_LOCK
        } else {
            KeyboardLedStatus::empty()
        };

        if let Some(myself) = Self::get_this(this) {
            let mut inner = myself.inner.borrow_mut();
            inner
                .events
                .dispatch(WindowEvent::AdviseModifiersLedStatus(modifiers, leds));
        }
    }

    extern "C" fn key_down(this: &mut Object, _sel: Sel, nsevent: id) {
        Self::key_common(this, nsevent, true);
    }

    extern "C" fn key_up(this: &mut Object, _sel: Sel, nsevent: id) {
        Self::key_common(this, nsevent, false);
    }

    extern "C" fn did_change_screen(this: &mut Object, _sel: Sel, _notification: id) {
        log::trace!("did_change_screen");
        if let Some(this) = Self::get_this(this) {
            // Just set a flag; we don't want to react immediately
            // as this even fires as part of a live move and the
            // resize flow may try to re-position the window to
            // the wrong place.
            this.inner.borrow_mut().screen_changed = true;
        }
    }

    extern "C" fn will_start_live_resize(this: &mut Object, _sel: Sel, _notification: id) {
        if let Some(this) = Self::get_this(this) {
            let mut inner = this.inner.borrow_mut();
            inner.live_resizing = true;
        }
    }

    extern "C" fn did_end_live_resize(this: &mut Object, _sel: Sel, _notification: id) {
        if let Some(this) = Self::get_this(this) {
            let mut inner = this.inner.borrow_mut();
            inner.live_resizing = false;
        }
    }

    extern "C" fn did_resize(this: &mut Object, _sel: Sel, _notification: id) {
        if let Some(this) = Self::get_this(this) {
            let inner = this.inner.borrow_mut();

            if let Some(gl_context_pair) = inner.gl_context_pair.as_ref() {
                gl_context_pair.backend.update();
            }
        }

        let frame = unsafe { NSView::frame(this as *mut _) };
        let backing_frame = unsafe { NSView::convertRectToBacking(this as *mut _, frame) };
        let width = backing_frame.size.width;
        let height = backing_frame.size.height;
        if let Some(this) = Self::get_this(this) {
            let mut inner = this.inner.borrow_mut();

            // This is a little gross; ideally we'd call
            // WindowInner:is_fullscreen to determine this, but
            // we can't get a mutable reference to it from here
            // as we can be called in a context where something
            // higher up the callstack already has a mutable
            // reference and we'd panic.
            let is_full_screen = inner.fullscreen.is_some()
                || inner.window.as_ref().map_or(false, |window| {
                    let window = window.load();
                    let style_mask = unsafe { NSWindow::styleMask(*window) };
                    style_mask.contains(NSWindowStyleMask::NSFullScreenWindowMask)
                });

            let live_resizing = inner.live_resizing;

            // Note: isZoomed can falsely return YES in situations such as
            // the current screen changing. We cannot detect that case here.
            // There is some logic to compensate for this in
            // wezterm-gui/src/termwindow/resize.rs.
            // <https://github.com/wezterm/wezterm/issues/3503>
            let is_zoomed = !is_full_screen
                && inner.window.as_ref().map_or(false, |window| {
                    let window = window.load();
                    unsafe { msg_send![*window, isZoomed] }
                });

            let window_level = inner
                .window
                .as_ref()
                .map(|window| {
                    let level = unsafe { window.load().level() };
                    nswindow_level_to_window_level(level)
                })
                .unwrap_or_default();

            let level_state = match window_level {
                WindowLevel::AlwaysOnBottom => WindowState::ALWAYS_ON_BOTTOM,
                WindowLevel::AlwaysOnTop => WindowState::ALWAYS_ON_TOP,
                WindowLevel::Normal => WindowState::default(),
            };

            let screen_state = match (is_full_screen, is_zoomed) {
                (true, _) => WindowState::FULL_SCREEN,
                (_, true) => WindowState::MAXIMIZED,
                _ => WindowState::default(),
            };
            if let Some(window) = inner.window.as_ref() {
                let window = window.load();
                if let (true, Some(view)) = (
                    inner.titlebar_sidebar_button_visible && !is_full_screen,
                    inner.view_id.as_ref().map(|view| view.load()),
                ) {
                    install_thinkterm_titlebar_sidebar_button(&window, *view);
                } else {
                    remove_thinkterm_titlebar_sidebar_button(&window);
                }
            }

            let dpi = inner
                .window
                .as_ref()
                .and_then(|window| {
                    let window = window.load();
                    dpi_for_window_screen(*window, &inner.config)
                })
                .unwrap_or(crate::DEFAULT_DPI * (backing_frame.size.width / frame.size.width))
                as usize;

            inner.events.dispatch(WindowEvent::Resized {
                dimensions: Dimensions {
                    pixel_width: width as usize,
                    pixel_height: height as usize,
                    dpi,
                },
                window_state: screen_state | level_state,
                live_resizing,
            });
        }
    }

    extern "C" fn update_layer(_view: &mut Object, _sel: Sel) {
        log::trace!("update_layer called");
    }

    extern "C" fn wants_update_layer(_view: &mut Object, _sel: Sel) -> BOOL {
        log::trace!("wants_update_layer called");
        YES
    }

    extern "C" fn display_layer(view: &mut Object, sel: Sel, _layer_id: id) {
        Self::draw_rect(
            view,
            sel,
            NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.)),
        )
    }

    extern "C" fn draw_layer_in_context(
        _view: &mut Object,
        _sel: Sel,
        _layer_id: id,
        _context: id,
    ) {
    }

    extern "C" fn layer_should_inherit_contents_scale_from_window(
        _: &Object,
        _: Sel,
        layer: *mut Object,
        _: CGFloat,
        _: *mut Object,
    ) -> BOOL {
        log::trace!("layer_should_inherit_contents_scale_from_window");
        unsafe {
            let () = msg_send![layer, setContentsScale: 1.0];
        }
        YES
    }

    extern "C" fn make_backing_layer(view: &mut Object, _: Sel) -> id {
        log::trace!("make_backing_layer");
        let class = class!(CAMetalLayer);
        unsafe {
            // Use type method to get a instance of CAMetalLayer.
            // So that we don't have to worry about retaining/releasing it.
            let layer: id = msg_send![class, layer];
            let () = msg_send![layer, setDelegate: view];
            let () = msg_send![layer, setContentsScale: 1.0];
            let () = msg_send![layer, setOpaque: NO];
            layer
        }
    }

    extern "C" fn draw_rect(view: &mut Object, sel: Sel, _dirty_rect: NSRect) {
        if let Some(this) = Self::get_this(view) {
            let Ok(mut inner) = this.inner.try_borrow_mut() else {
                // We're being asked to draw reentrantly while some other
                // handler holds the window state. AppKit has already cleared
                // needsDisplay for this pass, so if we simply return here the
                // frame is lost and the window stays stale until the next
                // interaction sets needsDisplay again. Re-arm it for the next
                // runloop turn instead.
                log::trace!("skipping draw while window is busy; re-arming");
                this.invalidated.set(true);
                let view_ptr: id = view as *mut Object;
                unsafe {
                    let () = msg_send![view_ptr, performSelector: sel!(thinktermRearmNeedsDisplay)
                                       withObject: nil
                                       afterDelay: 0.0];
                }
                return;
            };

            if inner.screen_changed {
                // If the screen resolution changed (which can also
                // happen if the window was dragged to another monitor
                // with different dpi), then we treat this as a resize
                // event that will in turn trigger an invalidation
                // and a repaint.
                inner.screen_changed = false;
                drop(inner);
                Self::did_resize(view, sel, nil);
                return;
            }

            // The throttle is only honored while its clearing timer can still
            // plausibly be pending. No legal frame period reaches anywhere
            // near this bound, so blowing past it means the timer is lost and
            // waiting on it would freeze the window permanently.
            const PAINT_THROTTLE_WATCHDOG: std::time::Duration =
                std::time::Duration::from_millis(250);
            let throttle_holds = this.paint_throttled.get()
                && this
                    .paint_throttled_since
                    .get()
                    .is_some_and(|since| since.elapsed() < PAINT_THROTTLE_WATCHDOG);
            if this.paint_throttled.get() && !throttle_holds {
                log::warn!("paint throttle outlived its clearing timer; breaking the latch");
            }
            if throttle_holds {
                this.invalidated.set(true);
            } else {
                let now = Instant::now();
                if let Some(last) = inner.last_repaint_time.replace(now) {
                    if thinkterm_perf_enabled() {
                        log::info!(
                            "thinkterm_perf macos_frame_interval_ms={:.2} win={}",
                            now.saturating_duration_since(last).as_secs_f64() * 1000.0,
                            inner.window_id,
                        );
                    }
                }
                inner.events.dispatch(WindowEvent::NeedRepaint);
                this.invalidated.set(false);
                this.paint_throttled.set(true);
                this.paint_throttled_since.set(Some(now));

                let window_id = inner.window_id;
                let max_fps = target_frame_fps(inner.config.max_fps);
                if thinkterm_perf_enabled() {
                    log::info!("thinkterm_perf macos_target_fps={max_fps:.0}");
                }
                let Some(remaining) = frame_throttle_delay(inner.config.max_fps, now.elapsed())
                else {
                    // Nothing left to enforce; hand the pacing back to the
                    // display, which is the only clock that matters here.
                    this.paint_throttled.set(false);
                    if this.invalidated.get() {
                        // Through `performSelector` rather than setting the flag
                        // inline: AppKit clears `needsDisplay` around this pass,
                        // so a request made from inside it can be lost.
                        if let Some(view_id) = inner.view_id.as_ref().map(|view| view.load()) {
                            unsafe {
                                let () = msg_send![*view_id, performSelector: sel!(thinktermRearmNeedsDisplay)
                                                   withObject: nil
                                                   afterDelay: 0.0];
                            }
                        }
                    }
                    return;
                };
                promise::spawn::spawn(async move {
                    async_io::Timer::after(remaining).await;
                    Connection::with_window_inner(window_id, move |inner| {
                        if let Some(window_view) = WindowView::get_this(unsafe { &**inner.view }) {
                            window_view.paint_throttled.set(false);
                            if window_view.invalidated.get() {
                                unsafe {
                                    let () = msg_send![*inner.view, setNeedsDisplay: YES];
                                }
                            }
                        }
                        Ok(())
                    });
                })
                .detach();
            }
        }
    }

    extern "C" fn rearm_needs_display(this: &mut Object, _sel: Sel) {
        unsafe {
            let () = msg_send![this, setNeedsDisplay: YES];
        }
    }

    /// Where the drag is, in the same window-relative backing pixels as
    /// `MouseEvent::coords`. `draggingLocation` is in window coordinates just
    /// like `locationInWindow`, so this mirrors `mouse_common` exactly; the
    /// view is flipped, so no y adjustment is needed.
    fn dragging_coords(this: &mut Object, sender: id) -> Point {
        let view = this as id;
        unsafe {
            let location: NSPoint = msg_send![sender, draggingLocation];
            let point = NSView::convertPoint_fromView_(view, location, nil);
            let rect = NSRect::new(NSPoint::new(0., 0.), NSSize::new(point.x, point.y));
            let backing_rect = NSView::convertRectToBacking(view, rect);
            // backing_rect computes abs() values, so restore the sign from
            // the original point.
            Point::new(
                f64::copysign(backing_rect.size.width, point.x) as isize,
                f64::copysign(backing_rect.size.height, point.y) as isize,
            )
        }
    }

    fn dragged_paths(sender: id) -> Option<Vec<PathBuf>> {
        let pb: id = unsafe { msg_send![sender, draggingPasteboard] };
        if pb.is_null() {
            return None;
        }
        let filenames =
            unsafe { NSPasteboard::propertyListForType(pb, appkit::NSFilenamesPboardType) };
        if filenames.is_null() {
            return None;
        }
        Some(
            unsafe { filenames.iter() }
                .map(|file| unsafe { PathBuf::from(nsstring_to_str(file)) })
                .collect(),
        )
    }

    fn dragging_hover(this: &mut Object, sender: id) -> NSUInteger {
        let Some(paths) = Self::dragged_paths(sender) else {
            return NS_DRAG_OPERATION_NONE;
        };
        let coords = Self::dragging_coords(this, sender);
        if let Some(this) = Self::get_this(this) {
            this.inner
                .borrow_mut()
                .events
                .dispatch(WindowEvent::DraggedFile {
                    paths,
                    coords: Some(coords),
                });
        }
        NS_DRAG_OPERATION_COPY
    }

    extern "C" fn dragging_entered(this: &mut Object, _: Sel, sender: id) -> NSUInteger {
        Self::dragging_hover(this, sender)
    }

    /// Registered so the hover position keeps arriving as the pointer moves.
    /// Without it AppKit reuses `draggingEntered:`'s answer for the whole
    /// drag and never reports where the pointer went.
    extern "C" fn dragging_updated(this: &mut Object, _: Sel, sender: id) -> NSUInteger {
        Self::dragging_hover(this, sender)
    }

    extern "C" fn dragging_exited(this: &mut Object, _: Sel, _sender: id) {
        if let Some(this) = Self::get_this(this) {
            this.inner
                .borrow_mut()
                .events
                .dispatch(WindowEvent::DragLeave);
        }
    }

    extern "C" fn perform_drag_operation(this: &mut Object, _: Sel, sender: id) -> BOOL {
        let Some(paths) = Self::dragged_paths(sender) else {
            return NO;
        };
        let coords = Self::dragging_coords(this, sender);
        if let Some(this) = Self::get_this(this) {
            this.inner
                .borrow_mut()
                .events
                .dispatch(WindowEvent::DroppedFile {
                    paths,
                    coords: Some(coords),
                });
        }
        YES
    }

    fn get_this(this: &Object) -> Option<&mut Self> {
        unsafe {
            let myself: *mut c_void = *this.get_ivar(VIEW_CLS_NAME);
            if myself.is_null() {
                None
            } else {
                Some(&mut *(myself as *mut Self))
            }
        }
    }

    fn enqueue_notification(&self, notification: PendingWindowNotification, view: id) {
        self.pending_notifications
            .borrow_mut()
            .enqueue(notification);
        self.try_drain_notifications(view);
    }

    fn try_drain_notifications(&self, view: id) {
        let schedule_retry =
            drain_pending_notifications(&self.pending_notifications, |notification| {
                let Ok(mut inner) = self.inner.try_borrow_mut() else {
                    return Err(notification);
                };
                inner
                    .events
                    .dispatch(WindowEvent::Notification(notification));
                Ok(())
            });
        if schedule_retry {
            let pending = self.pending_notifications.borrow().pending.len();
            log::trace!(
                "window is busy; queued {pending} notification(s) for the next runloop turn"
            );
            unsafe {
                let () = msg_send![view, performSelector: sel!(thinktermDrainNotifications)
                                   withObject: nil
                                   afterDelay: 0.0];
            }
        }
    }

    extern "C" fn drain_notifications(this: &mut Object, _sel: Sel) {
        let view: id = this as *mut Object;
        if let Some(window_view) = Self::get_this(this) {
            window_view
                .pending_notifications
                .borrow_mut()
                .begin_scheduled_drain();
            window_view.try_drain_notifications(view);
        }
    }

    fn init_with_frame(inner: &Rc<RefCell<Inner>>, rect: NSRect) -> anyhow::Result<StrongPtr> {
        let cls = Self::get_class();

        let view_id: id = unsafe { msg_send![cls, alloc] };
        let view_id: StrongPtr = unsafe { StrongPtr::new(msg_send![view_id, initWithFrame:rect]) };
        inner.borrow_mut().view_id.replace(view_id.weak());

        let view = Box::into_raw(Box::new(Self {
            inner: Rc::clone(&inner),
            pending_notifications: RefCell::new(PendingNotificationQueue::default()),
            paint_throttled: Cell::new(false),
            paint_throttled_since: Cell::new(None),
            invalidated: Cell::new(true),
        }));

        unsafe {
            (**view_id).set_ivar(VIEW_CLS_NAME, view as *mut c_void);
            (**view_id).set_ivar::<i64>(CURSOR_IVAR, 0);
        }

        Ok(view_id)
    }

    fn get_class() -> &'static Class {
        Class::get(VIEW_CLS_NAME).unwrap_or_else(Self::define_class)
    }

    fn define_class() -> &'static Class {
        let mut cls = ClassDecl::new(VIEW_CLS_NAME, class!(NSView))
            .expect("Unable to register WindowView class");

        cls.add_ivar::<*mut c_void>(VIEW_CLS_NAME);
        cls.add_ivar::<i64>(CURSOR_IVAR);
        cls.add_protocol(
            Protocol::get("NSTextInputClient").expect("failed to get NSTextInputClient protocol"),
        );

        cls.add_protocol(Protocol::get("CALayerDelegate").expect("CALayerDelegate not defined"));

        unsafe {
            cls.add_method(
                sel!(dealloc),
                WindowView::dealloc as extern "C" fn(&mut Object, Sel),
            );

            cls.add_method(
                sel!(weztermPerformKeyAssignment:),
                Self::wezterm_perform_key_assignment
                    as extern "C" fn(&mut Object, Sel, *mut Object),
            );
            cls.add_method(
                sel!(thinktermToggleWorkspaceSidebar:),
                Self::thinkterm_toggle_workspace_sidebar as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(thinktermRearmNeedsDisplay),
                Self::rearm_needs_display as extern "C" fn(&mut Object, Sel),
            );
            cls.add_method(
                sel!(thinktermDrainNotifications),
                Self::drain_notifications as extern "C" fn(&mut Object, Sel),
            );
            cls.add_method(
                sel!(windowWillClose:),
                Self::window_will_close as extern "C" fn(&mut Object, Sel, id),
            );

            cls.add_method(
                sel!(windowShouldClose:),
                Self::window_should_close as extern "C" fn(&mut Object, Sel, id) -> BOOL,
            );

            cls.add_method(
                sel!(makeBackingLayer),
                Self::make_backing_layer as extern "C" fn(&mut Object, Sel) -> id,
            );

            cls.add_method(
                sel!(layer:shouldInheritContentsScale:fromWindow:),
                Self::layer_should_inherit_contents_scale_from_window
                    as extern "C" fn(&Object, Sel, *mut Object, CGFloat, *mut Object) -> BOOL,
            );

            cls.add_method(
                sel!(drawRect:),
                Self::draw_rect as extern "C" fn(&mut Object, Sel, NSRect),
            );

            cls.add_method(
                sel!(updateLayer),
                Self::update_layer as extern "C" fn(&mut Object, Sel),
            );

            cls.add_method(
                sel!(wantsUpdateLayer),
                Self::wants_update_layer as extern "C" fn(&mut Object, Sel) -> BOOL,
            );

            cls.add_method(
                sel!(displayLayer:),
                Self::display_layer as extern "C" fn(&mut Object, Sel, id),
            );

            cls.add_method(
                sel!(drawLayer:inContext:),
                Self::draw_layer_in_context as extern "C" fn(&mut Object, Sel, id, id),
            );

            cls.add_method(
                sel!(isFlipped),
                Self::is_flipped as extern "C" fn(&Object, Sel) -> BOOL,
            );

            cls.add_method(
                sel!(isOpaque),
                Self::is_opaque as extern "C" fn(&Object, Sel) -> BOOL,
            );

            cls.add_method(
                sel!(allowsAutomaticWindowTabbing),
                Self::allow_automatic_tabbing as extern "C" fn(&Object, Sel) -> BOOL,
            );

            cls.add_method(
                sel!(windowWillStartLiveResize:),
                Self::will_start_live_resize as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(windowDidEndLiveResize:),
                Self::did_end_live_resize as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(windowDidResize:),
                Self::did_resize as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(windowDidChangeScreen:),
                Self::did_change_screen as extern "C" fn(&mut Object, Sel, id),
            );

            cls.add_method(
                sel!(windowDidBecomeKey:),
                Self::did_become_key as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(windowDidResignKey:),
                Self::did_resign_key as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(windowDidChangeOcclusionState:),
                Self::did_change_occlusion_state as extern "C" fn(&mut Object, Sel, id),
            );

            cls.add_method(
                sel!(mouseMoved:),
                Self::mouse_moved_or_dragged as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(mouseDragged:),
                Self::mouse_moved_or_dragged as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(rightMouseDragged:),
                Self::mouse_moved_or_dragged as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(mouseDown:),
                Self::mouse_down as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(mouseUp:),
                Self::mouse_up as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(rightMouseDown:),
                Self::right_mouse_down as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(rightMouseUp:),
                Self::right_mouse_up as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(otherMouseDragged:),
                Self::mouse_moved_or_dragged as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(otherMouseDown:),
                Self::other_mouse_down as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(otherMouseUp:),
                Self::other_mouse_up as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(scrollWheel:),
                Self::scroll_wheel as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(wantsScrollEventsForSwipeTrackingOnAxis:),
                Self::wants_scroll_events_for_swipe_tracking
                    as extern "C" fn(&Object, Sel, NSInteger) -> BOOL,
            );
            cls.add_method(
                sel!(mouseExited:),
                Self::mouse_exited as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(mouseEntered:),
                Self::mouse_entered as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(resetCursorRects),
                Self::reset_cursor_rects as extern "C" fn(&mut Object, Sel),
            );

            cls.add_method(
                sel!(keyDown:),
                Self::key_down as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(keyUp:),
                Self::key_up as extern "C" fn(&mut Object, Sel, id),
            );

            cls.add_method(
                sel!(performKeyEquivalent:),
                Self::perform_key_equivalent as extern "C" fn(&mut Object, Sel, id) -> BOOL,
            );

            cls.add_method(
                sel!(acceptsFirstResponder),
                Self::accepts_first_responder as extern "C" fn(&mut Object, Sel) -> BOOL,
            );

            cls.add_method(
                sel!(acceptsFirstMouse:),
                Self::accepts_first_mouse as extern "C" fn(&mut Object, Sel, id) -> BOOL,
            );

            cls.add_method(
                sel!(viewDidChangeEffectiveAppearance),
                Self::view_did_change_effective_appearance as extern "C" fn(&mut Object, Sel),
            );

            cls.add_method(
                sel!(updateTrackingAreas),
                Self::update_tracking_areas as extern "C" fn(&mut Object, Sel),
            );

            cls.add_method(
                sel!(flagsChanged:),
                Self::flags_changed as extern "C" fn(&mut Object, Sel, id),
            );

            // NSTextInputClient

            cls.add_method(
                sel!(hasMarkedText),
                Self::has_marked_text as extern "C" fn(&mut Object, Sel) -> BOOL,
            );
            cls.add_method(
                sel!(markedRange),
                Self::marked_range as extern "C" fn(&mut Object, Sel) -> NSRange,
            );
            cls.add_method(
                sel!(selectedRange),
                Self::selected_range as extern "C" fn(&mut Object, Sel) -> NSRange,
            );
            cls.add_method(
                sel!(setMarkedText:selectedRange:replacementRange:),
                Self::set_marked_text_selected_range_replacement_range
                    as extern "C" fn(&mut Object, Sel, id, NSRange, NSRange),
            );
            cls.add_method(
                sel!(unmarkText),
                Self::unmark_text as extern "C" fn(&mut Object, Sel),
            );
            cls.add_method(
                sel!(validAttributesForMarkedText),
                Self::valid_attributes_for_marked_text as extern "C" fn(&mut Object, Sel) -> id,
            );
            cls.add_method(
                sel!(doCommandBySelector:),
                Self::do_command_by_selector as extern "C" fn(&mut Object, Sel, Sel),
            );

            cls.add_method(
                sel!( attributedSubstringForProposedRange:actualRange:),
                Self::attributed_substring_for_proposed_range
                    as extern "C" fn(&mut Object, Sel, NSRange, NSRangePointer) -> id,
            );
            cls.add_method(
                sel!(insertText:replacementRange:),
                Self::insert_text_replacement_range as extern "C" fn(&mut Object, Sel, id, NSRange),
            );

            cls.add_method(
                sel!(characterIndexForPoint:),
                Self::character_index_for_point
                    as extern "C" fn(&mut Object, Sel, NSPoint) -> NSUInteger,
            );
            cls.add_method(
                sel!(firstRectForCharacterRange:actualRange:),
                Self::first_rect_for_character_range
                    as extern "C" fn(&mut Object, Sel, NSRange, NSRangePointer) -> NSRect,
            );
            cls.add_method(
                sel!(draggingEntered:),
                Self::dragging_entered as extern "C" fn(&mut Object, Sel, id) -> NSUInteger,
            );
            cls.add_method(
                sel!(draggingUpdated:),
                Self::dragging_updated as extern "C" fn(&mut Object, Sel, id) -> NSUInteger,
            );
            cls.add_method(
                sel!(draggingExited:),
                Self::dragging_exited as extern "C" fn(&mut Object, Sel, id),
            );
            cls.add_method(
                sel!(performDragOperation:),
                Self::perform_drag_operation as extern "C" fn(&mut Object, Sel, id) -> BOOL,
            );
        }

        cls.register()
    }
}
