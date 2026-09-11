use super::*;
use crate::connection::ConnectionOps;
use crate::parameters::{self, Parameters};
use crate::{
    Appearance, Clipboard, ClipboardContents, DeadKeyStatus, Dimensions, FolderPickerOptions,
    Handled, KeyCode, KeyEvent, Modifiers, MouseButtons, MouseCursor, MouseEvent, MouseEventKind,
    MousePress, Point, RawKeyEvent, Rect, RequestedWindowGeometry, ResolvedGeometry, ScreenPoint,
    ScreenRect, ULength, WindowDecorations, WindowEvent, WindowEventSender, WindowOps, WindowState,
};
use anyhow::{anyhow, bail};
use async_trait::async_trait;
use config::{ConfigHandle, ImePreeditRendering, SystemBackdrop};
use lazy_static::lazy_static;
use promise::Future;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, Win32WindowHandle, WindowHandle, WindowsDisplayHandle,
};
use shared_library::shared_library;
use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::TryInto;
use std::ffi::OsString;
use std::io::{self, Error as IoError};
use std::num::NonZeroIsize;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::rc::Rc;
use wezterm_color_types::LinearRgba;
use wezterm_font::FontConfiguration;
use wezterm_input_types::KeyboardLedStatus;
use winapi::shared::minwindef::*;
use winapi::shared::ntdef::*;
use winapi::shared::windef::*;
use winapi::shared::winerror::{
    ERROR_CANCELLED, FAILED, HRESULT_FROM_WIN32, RPC_E_CHANGED_MODE, S_FALSE, S_OK,
};
use winapi::shared::wtypesbase::CLSCTX_INPROC_SERVER;
use winapi::um::combaseapi::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize};
use winapi::um::imm::*;
use winapi::um::libloaderapi::GetModuleHandleW;
use winapi::um::objbase::{COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use winapi::um::shellapi::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use winapi::um::shellscalingapi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use winapi::um::shobjidl::{
    IFileOpenDialog, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_PATHMUSTEXIST,
    FOS_PICKFOLDERS,
};
use winapi::um::shobjidl_core::{CLSID_FileOpenDialog, IShellItem, SIGDN_FILESYSPATH};
use winapi::um::shtypes::COMDLG_FILTERSPEC;
use winapi::um::sysinfoapi::{GetTickCount, GetVersionExW};
use winapi::um::uxtheme::SetWindowTheme;
use winapi::um::wingdi::MAKEPOINTS;
use winapi::um::winnt::OSVERSIONINFOW;
use winapi::um::winuser::*;
use winapi::Interface;
use windows::UI::Color as WUIColor;
use windows::UI::ViewManagement::{UIColorType, UISettings};
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const GCS_RESULTSTR: DWORD = 0x800;
const GCS_COMPSTR: DWORD = 0x8;
const ISC_SHOWUICOMPOSITIONWINDOW: DWORD = 0x80000000;

#[allow(non_snake_case)]
#[repr(C)]
pub struct CANDIDATEFORM {
    dwIndex: DWORD,
    dwStyle: DWORD,
    ptCurrentPos: POINT,
    rcArea: RECT,
}
pub type LPCANDIDATEFORM = *mut CANDIDATEFORM;

extern "system" {
    pub fn ImmGetCompositionStringW(himc: HIMC, index: DWORD, buf: LPVOID, buflen: DWORD) -> LONG;
    pub fn ImmSetCandidateWindow(himc: HIMC, lpCandidate: LPCANDIDATEFORM) -> BOOL;
}

lazy_static! {
    static ref IS_WIN10: bool = {
        let osver = OSVERSIONINFOW {
            dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as _,
            ..Default::default()
        };

        if unsafe { GetVersionExW(&osver as *const _ as _) } == winapi::shared::minwindef::TRUE {
            osver.dwBuildNumber < 22000
        } else {
            true
        }
    };
    static ref IS_WIN11_22H2: bool = {
        let osver = OSVERSIONINFOW {
            dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as _,
            ..Default::default()
        };

        if unsafe { GetVersionExW(&osver as *const _ as _) } == winapi::shared::minwindef::TRUE {
            osver.dwBuildNumber >= 22621
        } else {
            true
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub(crate) struct HWindow(HWND);
unsafe impl Send for HWindow {}
unsafe impl Sync for HWindow {}

pub(crate) struct WindowInner {
    /// Non-owning reference to the window handle
    hwnd: HWindow,
    events: WindowEventSender,
    gl_state: Option<Rc<glium::backend::Context>>,
    /// Fraction of mouse scroll
    hscroll_remainder: i16,
    vscroll_remainder: i16,

    last_size: Option<Dimensions>,
    in_size_move: bool,
    dead_pending: Option<(Modifiers, u32)>,
    saved_placement: Option<WINDOWPLACEMENT>,
    track_mouse_leave: bool,
    window_drag_position: Option<ScreenPoint>,
    maximize_button_position: Option<ScreenRect>,

    keyboard_info: KeyboardLayoutInfo,
    appearance: Appearance,

    config: ConfigHandle,
    paint_throttled: bool,
    invalidated: bool,
    /// Last effective visibility sent to the platform-neutral window layer.
    /// WM_WINDOWPOSCHANGED is noisy, so only edges should restart its timers.
    occlusion_visible: Option<bool>,
    /// Last restore geometry reported as `WindowFrameChanged`, for the same
    /// reason as `occlusion_visible`: the owner wants the edges, not a message
    /// per pixel of a drag.
    last_frame_event: Option<(ScreenRect, bool)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct Window(HWindow);

fn wuicolor_to_linearrgba(color: WUIColor) -> LinearRgba {
    LinearRgba::with_srgba(color.R, color.G, color.B, 255)
}

fn rect_width(r: &RECT) -> i32 {
    r.right - r.left
}

fn hresult_to_result(hr: HRESULT, context: &str) -> anyhow::Result<()> {
    if FAILED(hr) {
        anyhow::bail!("{context} failed with HRESULT {:#x}", hr as u32);
    }
    Ok(())
}

fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn pathbuf_from_shell_string(raw_path: LPWSTR) -> Option<PathBuf> {
    if raw_path.is_null() {
        return None;
    }

    let mut len = 0;
    while *raw_path.add(len) != 0 {
        len += 1;
    }

    let path = OsString::from_wide(std::slice::from_raw_parts(raw_path, len));
    Some(PathBuf::from(path))
}

unsafe fn pick_folder_dialog(
    hwnd: HWND,
    picker_options: &FolderPickerOptions,
) -> anyhow::Result<Option<PathBuf>> {
    let coinit = CoInitializeEx(
        null_mut(),
        COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE,
    );
    let should_uninitialize = coinit == S_OK || coinit == S_FALSE;
    if FAILED(coinit) && coinit != RPC_E_CHANGED_MODE {
        hresult_to_result(coinit, "CoInitializeEx")?;
    }

    let result = (|| {
        let mut dialog: *mut IFileOpenDialog = null_mut();
        hresult_to_result(
            CoCreateInstance(
                &CLSID_FileOpenDialog,
                null_mut(),
                CLSCTX_INPROC_SERVER,
                &IFileOpenDialog::uuidof(),
                &mut dialog as *mut _ as *mut _,
            ),
            "CoCreateInstance(FileOpenDialog)",
        )?;

        let title = wide_null(&picker_options.title);
        let open = wide_null(&picker_options.prompt);
        let mut selected_path = None;
        let mut result_item: *mut IShellItem = null_mut();
        let mut raw_path: LPWSTR = null_mut();

        let dialog_result = (|| {
            let mut options = std::mem::zeroed();
            hresult_to_result(
                (*dialog).GetOptions(&mut options),
                "IFileDialog::GetOptions",
            )?;
            let options = std::mem::transmute(
                options as u32
                    | FOS_PICKFOLDERS as u32
                    | FOS_FORCEFILESYSTEM as u32
                    | FOS_PATHMUSTEXIST as u32
                    | FOS_NOCHANGEDIR as u32,
            );
            hresult_to_result((*dialog).SetOptions(options), "IFileDialog::SetOptions")?;
            hresult_to_result((*dialog).SetTitle(title.as_ptr()), "IFileDialog::SetTitle")?;
            hresult_to_result(
                (*dialog).SetOkButtonLabel(open.as_ptr()),
                "IFileDialog::SetOkButtonLabel",
            )?;

            let show_result = (*dialog).Show(hwnd);
            if show_result == HRESULT_FROM_WIN32(ERROR_CANCELLED) {
                return Ok(());
            }
            hresult_to_result(show_result, "IFileDialog::Show")?;

            hresult_to_result(
                (*dialog).GetResult(&mut result_item),
                "IFileDialog::GetResult",
            )?;
            hresult_to_result(
                (*result_item).GetDisplayName(SIGDN_FILESYSPATH, &mut raw_path),
                "IShellItem::GetDisplayName",
            )?;
            selected_path = pathbuf_from_shell_string(raw_path);
            Ok(())
        })();

        if !raw_path.is_null() {
            CoTaskMemFree(raw_path as *mut _);
        }
        if !result_item.is_null() {
            (*result_item).Release();
        }
        (*dialog).Release();

        dialog_result.map(|_| selected_path)
    })();

    if should_uninitialize {
        CoUninitialize();
    }

    result
}

unsafe fn pick_app_dialog(hwnd: HWND) -> anyhow::Result<Option<PathBuf>> {
    let coinit = CoInitializeEx(
        null_mut(),
        COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE,
    );
    let should_uninitialize = coinit == S_OK || coinit == S_FALSE;
    if FAILED(coinit) && coinit != RPC_E_CHANGED_MODE {
        hresult_to_result(coinit, "CoInitializeEx")?;
    }

    let result = (|| {
        let mut dialog: *mut IFileOpenDialog = null_mut();
        hresult_to_result(
            CoCreateInstance(
                &CLSID_FileOpenDialog,
                null_mut(),
                CLSCTX_INPROC_SERVER,
                &IFileOpenDialog::uuidof(),
                &mut dialog as *mut _ as *mut _,
            ),
            "CoCreateInstance(FileOpenDialog)",
        )?;

        let title = wide_null("Choose Application");
        let choose = wide_null("Choose");
        let filter_name = wide_null("Applications (*.exe)");
        let filter_spec = wide_null("*.exe");
        let filters = [COMDLG_FILTERSPEC {
            pszName: filter_name.as_ptr(),
            pszSpec: filter_spec.as_ptr(),
        }];
        let mut selected_path = None;
        let mut result_item: *mut IShellItem = null_mut();
        let mut raw_path: LPWSTR = null_mut();

        let dialog_result = (|| {
            let mut options = std::mem::zeroed();
            hresult_to_result(
                (*dialog).GetOptions(&mut options),
                "IFileDialog::GetOptions",
            )?;
            let options = std::mem::transmute(
                options as u32
                    | FOS_FORCEFILESYSTEM as u32
                    | FOS_PATHMUSTEXIST as u32
                    | FOS_FILEMUSTEXIST as u32
                    | FOS_NOCHANGEDIR as u32,
            );
            hresult_to_result((*dialog).SetOptions(options), "IFileDialog::SetOptions")?;
            hresult_to_result(
                (*dialog).SetFileTypes(filters.len() as u32, filters.as_ptr()),
                "IFileDialog::SetFileTypes",
            )?;
            hresult_to_result((*dialog).SetTitle(title.as_ptr()), "IFileDialog::SetTitle")?;
            hresult_to_result(
                (*dialog).SetOkButtonLabel(choose.as_ptr()),
                "IFileDialog::SetOkButtonLabel",
            )?;

            let show_result = (*dialog).Show(hwnd);
            if show_result == HRESULT_FROM_WIN32(ERROR_CANCELLED) {
                return Ok(());
            }
            hresult_to_result(show_result, "IFileDialog::Show")?;

            hresult_to_result(
                (*dialog).GetResult(&mut result_item),
                "IFileDialog::GetResult",
            )?;
            hresult_to_result(
                (*result_item).GetDisplayName(SIGDN_FILESYSPATH, &mut raw_path),
                "IShellItem::GetDisplayName",
            )?;
            selected_path = pathbuf_from_shell_string(raw_path);
            Ok(())
        })();

        if !raw_path.is_null() {
            CoTaskMemFree(raw_path as *mut _);
        }
        if !result_item.is_null() {
            (*result_item).Release();
        }
        (*dialog).Release();

        dialog_result.map(|_| selected_path)
    })();

    if should_uninitialize {
        CoUninitialize();
    }

    result
}

fn rect_height(r: &RECT) -> i32 {
    r.bottom - r.top
}

fn adjust_client_to_window_dimensions(
    style: u32,
    width: usize,
    height: usize,
    dpi: u32,
) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width as _,
        bottom: height as _,
    };
    unsafe { AdjustWindowRectExForDpi(&mut rect, style, 0, 0, dpi) };

    (rect_width(&rect), rect_height(&rect))
}

fn rc_to_pointer(arc: &Rc<RefCell<WindowInner>>) -> *const RefCell<WindowInner> {
    let cloned = Rc::clone(arc);
    Rc::into_raw(cloned)
}

fn rc_from_pointer(lparam: LPVOID) -> Rc<RefCell<WindowInner>> {
    // Turn it into an Rc
    let arc = unsafe { Rc::from_raw(std::mem::transmute(lparam)) };
    // Add a ref for the caller
    let cloned = Rc::clone(&arc);

    // We must not drop this ref though; turn it back into a raw pointer!
    let _ = Rc::into_raw(arc);

    cloned
}

fn rc_from_hwnd(hwnd: HWND) -> Option<Rc<RefCell<WindowInner>>> {
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as LPVOID };
    if raw.is_null() {
        None
    } else {
        Some(rc_from_pointer(raw))
    }
}

fn take_rc_from_pointer(lparam: LPVOID) -> Rc<RefCell<WindowInner>> {
    unsafe { Rc::from_raw(std::mem::transmute(lparam)) }
}

fn callback_behavior() -> glium::debug::DebugCallbackBehavior {
    if cfg!(debug_assertions) && false
    /* https://github.com/glium/glium/issues/1885 */
    {
        glium::debug::DebugCallbackBehavior::DebugMessageOnError
    } else {
        glium::debug::DebugCallbackBehavior::Ignore
    }
}

impl HasDisplayHandle for WindowInner {
    fn display_handle(&self) -> Result<DisplayHandle, HandleError> {
        unsafe {
            Ok(DisplayHandle::borrow_raw(RawDisplayHandle::Windows(
                WindowsDisplayHandle::new(),
            )))
        }
    }
}

impl HasWindowHandle for WindowInner {
    fn window_handle(&self) -> Result<WindowHandle, HandleError> {
        let mut handle =
            Win32WindowHandle::new(NonZeroIsize::new(self.hwnd.0 as _).expect("non-zero"));
        handle.hinstance = NonZeroIsize::new(unsafe { GetModuleHandleW(null()) } as _);
        unsafe { Ok(WindowHandle::borrow_raw(RawWindowHandle::Win32(handle))) }
    }
}

impl WindowInner {
    fn enable_opengl(&mut self) -> anyhow::Result<Rc<glium::backend::Context>> {
        let conn = Connection::get().unwrap();

        let gl_state = if self.config.prefer_egl {
            match conn.gl_connection.borrow().as_ref() {
                None => crate::egl::GlState::create(None, self.hwnd.0),
                Some(glconn) => {
                    crate::egl::GlState::create_with_existing_connection(glconn, self.hwnd.0)
                }
            }
        } else {
            Err(anyhow::anyhow!("Config says to avoid EGL"))
        }
        .and_then(|egl| unsafe {
            log::trace!("Initialized EGL!");
            conn.gl_connection
                .borrow_mut()
                .replace(Rc::clone(egl.get_connection()));
            let backend = Rc::new(egl);
            Ok(glium::backend::Context::new(
                backend,
                true,
                callback_behavior(),
            )?)
        })
        .or_else(|err| {
            log::trace!("EGL init failed {:?}, fall back to WGL", err);
            super::wgl::GlState::create(self.hwnd.0).and_then(|state| unsafe {
                Ok(glium::backend::Context::new(
                    Rc::new(state),
                    true,
                    callback_behavior(),
                )?)
            })
        })?;

        self.gl_state.replace(gl_state.clone());

        Ok(gl_state)
    }

    fn get_effective_dpi(&self) -> usize {
        effective_dpi_for_window(self.hwnd.0, &self.config)
    }

    /// Check if we need to generate a resize callback.
    /// Calls resize if needed.
    /// Returns true if we did.
    fn check_and_call_resize_if_needed(&mut self) -> bool {
        /*
        if self.gl_state.is_none() {
            // Don't cache state or generate resize callbacks until
            // we've set up opengl, otherwise we can miss propagating
            // some state during the initial window setup that results
            // in the window dimensions being out of sync with the dpi
            // when eg: the system display settings are set to 200%
            // scale factor.
            return false;
        }
        */

        let mut rect = RECT {
            left: 0,
            bottom: 0,
            right: 0,
            top: 0,
        };
        unsafe {
            GetClientRect(self.hwnd.0, &mut rect);
        }
        let pixel_width = rect_width(&rect) as usize;
        let pixel_height = rect_height(&rect) as usize;

        let current_dims = Dimensions {
            pixel_width,
            pixel_height,
            dpi: self.get_effective_dpi(),
        };

        let same = self
            .last_size
            .as_ref()
            .map(|&dims| dims == current_dims)
            .unwrap_or(false);
        self.last_size.replace(current_dims);

        if !same {
            self.set_ime_window_position(Rect::default());

            self.events.dispatch(WindowEvent::Resized {
                dimensions: current_dims,
                window_state: get_window_state(self.hwnd.0),
                live_resizing: self.in_size_move,
            });
        }

        !same
    }

    fn apply_decoration(&mut self) {
        let hwnd = self.hwnd.0;
        schedule_apply_decoration(hwnd, self.config.window_decorations);
    }
}

fn schedule_apply_decoration(hwnd: HWND, decorations: WindowDecorations) {
    promise::spawn::spawn(async move {
        apply_decoration_immediate(hwnd, decorations);
    })
    .detach();
}

/// Re-theme every window, after the application was asked to present a
/// different appearance. `apply_theme` reads that appearance for itself, and
/// its own comparison against the window's last-seen value decides whether an
/// `AppearanceChanged` is worth dispatching.
///
/// The handles are collected before any of them is themed: `apply_theme`
/// borrows each window's inner state and dispatches from inside that borrow,
/// which it cannot do while the window map is also borrowed here.
pub(crate) fn reapply_appearance_to_all_windows() {
    let conn = match Connection::get() {
        Some(conn) => conn,
        None => return,
    };
    let hwnds: Vec<HWND> = conn.windows.borrow().keys().map(|window| window.0).collect();
    for hwnd in hwnds {
        apply_theme(hwnd);
    }
}

fn apply_decoration_immediate(hwnd: HWND, decorations: WindowDecorations) {
    match rc_from_hwnd(hwnd) {
        Some(inner) => {
            if inner.borrow().saved_placement.is_some() {
                // We are full screen; ignore it for now
                return;
            }
        }
        None => return,
    };

    unsafe {
        let orig_style = GetWindowLongW(hwnd, GWL_STYLE);
        let style = decorations_to_style(decorations);
        let new_style = (orig_style & !(WS_OVERLAPPEDWINDOW as i32)) | style as i32;
        SetWindowLongW(hwnd, GWL_STYLE, new_style);
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE
                | SWP_NOMOVE
                | SWP_NOSIZE
                | SWP_NOZORDER
                | SWP_NOOWNERZORDER
                | SWP_FRAMECHANGED,
        );
        apply_theme(hwnd);
    }
}

fn decorations_to_style(decorations: WindowDecorations) -> u32 {
    if decorations == WindowDecorations::RESIZE
        || decorations == (WindowDecorations::RESIZE | WindowDecorations::INTEGRATED_BUTTONS)
        || decorations == WindowDecorations::INTEGRATED_BUTTONS
    {
        WS_OVERLAPPEDWINDOW
    } else if decorations == WindowDecorations::TITLE {
        WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX
    } else if decorations == WindowDecorations::NONE {
        WS_POPUP
    } else if decorations == WindowDecorations::TITLE | WindowDecorations::RESIZE {
        WS_OVERLAPPEDWINDOW
    } else {
        WS_OVERLAPPEDWINDOW
    }
}

/// The dpi our layout should use for a window sitting on its current monitor.
///
/// Split out of `WindowInner::get_effective_dpi` so that a window that does
/// not have its `WindowInner` wired up yet - one we have just created - reads
/// its dpi through exactly the same rules, `dpi` and `dpi_by_screen` overrides
/// included. Two copies of these rules would be two chances to disagree about
/// what a window's scale is.
fn effective_dpi_for_window(hwnd: HWND, config: &ConfigHandle) -> usize {
    let system_dpi = unsafe { GetDpiForWindow(hwnd) } as f64;

    // Per-monitor override, then the global override, then what the system
    // reports - the same precedence `ScreenInfoHelper::enumerate` applies when
    // it fills in `ScreenInfo::effective_dpi`. The two have to agree: sizing a
    // window reads this side while `default_dpi` reads the enumerate side, so
    // a disagreement surfaces as a window that resizes itself the moment it
    // opens. This used to fall straight through to the system dpi whenever
    // `dpi_by_screen` was non-empty but held no entry for the current monitor,
    // silently ignoring a global `dpi`.
    if !config.dpi_by_screen.is_empty() {
        unsafe {
            let mut mi: MONITORINFOEXW = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            GetMonitorInfoW(mon, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO);

            if let Ok(info) = crate::os::windows::connection::ScreenInfoHelper::new() {
                let name = info.monitor_name(&mi);
                if let Some(dpi) = config.dpi_by_screen.get(&name).copied() {
                    return dpi as usize;
                }
            }
        }
    }

    config.dpi.unwrap_or(system_dpi) as usize
}

/// `geometry` was measured against `creation_dpi` - the dpi of the monitor
/// [`ConnectionOps::default_dpi`] reported - but a `CW_USEDEFAULT` window opens
/// wherever Windows decides, which need not be that monitor. On a 200%/100%
/// pair that is a window at twice or half the size that was asked for, and
/// nothing later fixes it: the resize that follows re-rasterizes the fonts for
/// the new dpi but never revisits the dimensions.
///
/// Rescale the client area for where the window actually landed, and leave the
/// position alone (`SWP_NOMOVE`) so Windows keeps cascading new windows
/// instead of stacking them all in the middle of a monitor.
///
/// A pinned `dpi` needs no special case: `default_dpi` and
/// `effective_dpi_for_window` both return it, so the content scale below comes
/// out at 1.0 and only the frame is corrected. The window is still hidden at
/// this point - `ShowWindow` runs later - so there is no maximized or
/// minimized state to preserve.
fn resize_client_for_actual_monitor(
    hwnd: HWND,
    style: u32,
    config: &ConfigHandle,
    geometry: &ResolvedGeometry,
    creation_dpi: usize,
) {
    let content_dpi = effective_dpi_for_window(hwnd, config);
    if creation_dpi == 0 || content_dpi == 0 {
        return;
    }

    let scale = content_dpi as f64 / creation_dpi as f64;
    let client_width = ((geometry.width as f64 * scale).round() as usize).max(1);
    let client_height = ((geometry.height as f64 * scale).round() as usize).max(1);

    // The frame is Windows' to draw and it draws it at the monitor's real
    // scale, whatever dpi we may have pinned for our own layout. Handing
    // AdjustWindowRectExForDpi a config dpi would size the title bar and
    // borders for a scale the system is not using.
    let frame_dpi = unsafe { GetDpiForWindow(hwnd) };
    let (width, height) =
        adjust_client_to_window_dimensions(style, client_width, client_height, frame_dpi);

    // Compare against the window we actually got rather than against dpi
    // equality. With a 96dpi primary and a 192dpi secondary, a window that
    // lands on the secondary can have a content dpi that already matches while
    // its frame was still adjusted for the monitor guessed at creation - a
    // client area short by the frame delta, which dpi equality cannot see.
    let mut rect: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return;
    }
    if rect_width(&rect) == width && rect_height(&rect) == height {
        return;
    }

    log::trace!(
        "window was sized {}x{} for {creation_dpi}dpi but opened at content dpi \
         {content_dpi} / frame dpi {frame_dpi}; correcting to client \
         {client_width}x{client_height} (window {width}x{height})",
        geometry.width,
        geometry.height
    );

    unsafe {
        SetWindowPos(
            hwnd,
            null_mut(),
            0,
            0,
            width,
            height,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

fn monitor_dpi(mon: HMONITOR) -> u32 {
    // Left at the 96dpi default if the call fails, which is what
    // GetDpiForMonitor documents for an invalid monitor handle.
    let mut dpi_x = USER_DEFAULT_SCREEN_DPI as u32;
    let mut dpi_y = USER_DEFAULT_SCREEN_DPI as u32;
    if !mon.is_null() {
        unsafe { GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    }
    dpi_x
}

impl Window {
    fn create_window(
        config: ConfigHandle,
        class_name: &str,
        name: &str,
        geometry: ResolvedGeometry,
        frame_rect: Option<ScreenRect>,
        lparam: *const RefCell<WindowInner>,
    ) -> anyhow::Result<HWND> {
        let class_name = wide_string(class_name);
        let h_inst = unsafe { GetModuleHandleW(null()) };
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW | CS_OWNDC,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: h_inst,
            // FIXME: this resource is specific to the wezterm build and this should
            // really be made generic for other sorts of windows.
            // The ID is defined in assets/windows/resource.rc
            hIcon: unsafe { LoadIconW(h_inst, MAKEINTRESOURCEW(0x101)) },
            hCursor: null_mut(),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class_name.as_ptr(),
        };

        if unsafe { RegisterClassW(&class) } == 0 {
            let err = IoError::last_os_error();
            match err.raw_os_error() {
                Some(code)
                    if code == winapi::shared::winerror::ERROR_CLASS_ALREADY_EXISTS as i32 => {}
                _ => return Err(err.into()),
            }
        }

        let decorations = config.window_decorations;
        let style = decorations_to_style(decorations);

        // A remembered frame is already in the units CreateWindowExW wants --
        // an outer-frame rect in screen pixels -- so it goes straight through,
        // with none of the client-to-window growing or monitor guessing that
        // the rest of this function does. That is what makes the round trip
        // exact: what `GetWindowRect` gave us last time is what it gives us
        // again, with no frame thickness added and no dpi re-derivation.
        if let Some(frame) = frame_rect {
            let name = wide_string(name);
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    name.as_ptr(),
                    style,
                    frame.origin.x as i32,
                    frame.origin.y as i32,
                    frame.size.width as i32,
                    frame.size.height as i32,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    std::mem::transmute(lparam),
                )
            };

            if hwnd.is_null() {
                let err = IoError::last_os_error();
                bail!("CreateWindowExW: {}", err);
            }

            schedule_apply_decoration(hwnd, decorations);
            return Ok(hwnd);
        }

        // A WS_POPUP window has no system frame to place, so we choose its
        // position ourselves. Put it on the monitor `active_monitor` picks,
        // because that is the monitor whose dpi `default_dpi` reported when
        // the requested size was computed - see the note on `active_monitor`
        // for what sizing against one monitor and opening on another costs.
        // Decorated windows get CW_USEDEFAULT below, so the system chooses
        // the monitor and the primary stays the best guess available.
        let is_popup = (style & WS_POPUP) != 0;
        let target_monitor = if is_popup {
            active_monitor()
        } else {
            unsafe { MonitorFromWindow(null_mut(), MONITOR_DEFAULTTOPRIMARY) }
        };
        let frame_dpi = monitor_dpi(target_monitor);
        let (width, height) =
            adjust_client_to_window_dimensions(style, geometry.width, geometry.height, frame_dpi);

        let (x, y) = match (geometry.x, geometry.y) {
            (Some(x), Some(y)) => (x, y),
            _ => {
                if !is_popup {
                    (CW_USEDEFAULT, CW_USEDEFAULT)
                } else {
                    // WS_POPUP windows need to specify the initial position.
                    // We pick the middle of the monitor chosen above.
                    unsafe {
                        let mut mi: MONITORINFO = std::mem::zeroed();
                        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                        GetMonitorInfoW(target_monitor, &mut mi);

                        let mon_width = mi.rcMonitor.right - mi.rcMonitor.left;
                        let mon_height = mi.rcMonitor.bottom - mi.rcMonitor.top;

                        (
                            mi.rcMonitor.left + (mon_width - width) / 2,
                            mi.rcMonitor.top + (mon_height - height) / 2,
                        )
                    }
                }
            }
        };

        let name = wide_string(name);
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                name.as_ptr(),
                style,
                x,
                y,
                width,
                height,
                null_mut(),
                null_mut(),
                null_mut(),
                std::mem::transmute(lparam),
            )
        };

        if hwnd.is_null() {
            let err = IoError::last_os_error();
            bail!("CreateWindowExW: {}", err);
        }

        // We have to re-apply the styles otherwise they don't
        // completely stick
        schedule_apply_decoration(hwnd, decorations);

        Ok(hwnd)
    }

    pub async fn new_window<F>(
        class_name: &str,
        name: &str,
        geometry: RequestedWindowGeometry,
        config: Option<&ConfigHandle>,
        _font_config: Rc<FontConfiguration>,
        event_handler: F,
    ) -> anyhow::Result<Window>
    where
        F: 'static + FnMut(WindowEvent, &Window),
    {
        let events = WindowEventSender::new(event_handler);

        let config = match config {
            Some(c) => c.clone(),
            None => config::configuration(),
        };
        let appearance = get_appearance();

        let inner = Rc::new(RefCell::new(WindowInner {
            hwnd: HWindow(null_mut()),
            appearance,
            events,
            gl_state: None,
            vscroll_remainder: 0,
            hscroll_remainder: 0,
            keyboard_info: KeyboardLayoutInfo::new(),
            last_size: None,
            in_size_move: false,
            dead_pending: None,
            saved_placement: None,
            track_mouse_leave: false,
            window_drag_position: None,
            maximize_button_position: None,
            config: config.clone(),
            paint_throttled: false,
            invalidated: true,
            occlusion_visible: None,
            last_frame_event: None,
        }));

        // Careful: `raw` owns a ref to inner, but there is no Drop impl
        let raw = rc_to_pointer(&inner);

        let conn = Connection::get().expect("Connection::init was not called");

        // Captured here, next to the call that consumed it: `resolve_geometry`
        // measures the requested size against this dpi, and `create_window`
        // needs to know which monitor's scale the result assumed.
        let creation_dpi = conn.default_dpi() as usize;
        // Taken before `resolve_geometry` consumes the request: a remembered
        // frame bypasses the resolution entirely.
        let frame_rect = geometry.windows_frame_rect;
        let geometry = conn.resolve_geometry(geometry);

        let hwnd = match Self::create_window(
            config.clone(),
            class_name,
            name,
            geometry.clone(),
            frame_rect,
            raw,
        ) {
            Ok(hwnd) => HWindow(hwnd),
            Err(err) => {
                // Ensure that we drop the extra ref to raw before we return
                drop(unsafe { Rc::from_raw(raw) });
                return Err(err);
            }
        };
        let window_handle = Window(hwnd);
        inner
            .borrow_mut()
            .events
            .assign_window(window_handle.clone());

        // Deliberately after `assign_window`, not inside `create_window`.
        // `WindowEventSender::dispatch` drops events until it has a window to
        // hand the handler, while `check_and_call_resize_if_needed` records
        // the new size in `last_size` regardless - so correcting the size any
        // earlier would leave the owner believing the dimensions it asked for
        // while the later, real resize deduped itself away as "no change".
        // Skipped for a restored frame: that correction exists to re-derive a
        // window size from a requested *client* size once the real monitor is
        // known, and the restored rect is already the size we want, measured
        // on this same desktop the last time the window was open.
        if frame_rect.is_none() {
            resize_client_for_actual_monitor(
                hwnd.0,
                decorations_to_style(config.window_decorations),
                &config,
                &geometry,
                creation_dpi,
            );
        }

        apply_theme(hwnd.0);
        enable_blur_behind(hwnd.0);

        // Make window capable of accepting drag and drop
        unsafe {
            DragAcceptFiles(hwnd.0, winapi::shared::minwindef::TRUE);
        }

        conn.windows
            .borrow_mut()
            .insert(hwnd.clone(), Rc::clone(&inner));

        Ok(window_handle)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ShowWindowCommand {
    Normal,
    Minimize,
    Maximize,
}

fn schedule_show_window(hwnd: HWindow, show: ShowWindowCommand) {
    // ShowWindow can call to the window proc and may attempt
    // to lock inner, so we avoid locking it ourselves here
    log::trace!("scheduling ShowWindowCommand {show:?}");
    promise::spawn::spawn(async move {
        unsafe {
            log::trace!("applying ShowWindowCommand {show:?}");
            ShowWindow(
                hwnd.0,
                match show {
                    ShowWindowCommand::Normal => SW_NORMAL,
                    ShowWindowCommand::Minimize => SW_MINIMIZE,
                    ShowWindowCommand::Maximize => SW_MAXIMIZE,
                },
            );
        }
    })
    .detach();
}

impl WindowInner {
    fn close(&mut self) {
        let hwnd = self.hwnd;
        promise::spawn::spawn(async move {
            unsafe {
                DestroyWindow(hwnd.0);
            }
        })
        .detach();
    }

    fn set_cursor(&mut self, cursor: Option<MouseCursor>) {
        apply_mouse_cursor(cursor);
    }

    fn set_window_position(&self, coords: ScreenPoint) {
        let hwnd = self.hwnd.0;
        log::trace!("set_window_position wants {coords:?}");
        promise::spawn::spawn(async move {
            log::trace!("set_window_position apply {coords:?}");
            let mut rect = RECT {
                left: 0,
                bottom: 0,
                right: 0,
                top: 0,
            };
            unsafe {
                GetWindowRect(hwnd, &mut rect);

                let origin = client_to_screen(hwnd, Point::new(0, 0));
                let delta_x = origin.x as i32 - rect.left;
                let delta_y = origin.y as i32 - rect.top;

                MoveWindow(
                    hwnd,
                    coords.x as i32 - delta_x,
                    coords.y as i32 - delta_y,
                    rect_width(&rect),
                    rect_height(&rect),
                    1,
                );
            }
        })
        .detach();
    }

    fn set_title(&mut self, title: &str) {
        let title = wide_string(title);
        unsafe {
            SetWindowTextW(self.hwnd.0, title.as_ptr());
        }
    }

    fn set_text_cursor_position(&mut self, cursor: Rect) {
        self.set_ime_window_position(cursor);
    }

    fn set_ime_window_position(&mut self, cursor: Rect) {
        let imc = ImmContext::get(self.hwnd.0);
        match self.config.ime_preedit_rendering {
            ImePreeditRendering::Builtin => imc.set_candidate_window_position(cursor),
            ImePreeditRendering::System => imc.set_composition_window_position(cursor),
        }
    }

    fn config_did_change(&mut self, config: &ConfigHandle) {
        self.config = config.clone();
        self.apply_decoration();
    }

    fn toggle_fullscreen(&mut self) {
        unsafe {
            let hwnd = self.hwnd.0;
            let style = GetWindowLongW(hwnd, GWL_STYLE);
            let config = self.config.clone();
            if let Some(placement) = self.saved_placement.take() {
                promise::spawn::spawn(async move {
                    let style = decorations_to_style(config.window_decorations);
                    SetWindowLongW(hwnd, GWL_STYLE, style as i32);
                    SetWindowPlacement(hwnd, &placement);
                    SetWindowPos(
                        hwnd,
                        std::ptr::null_mut(),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE
                            | SWP_NOSIZE
                            | SWP_NOZORDER
                            | SWP_NOOWNERZORDER
                            | SWP_FRAMECHANGED,
                    );
                })
                .detach();
            } else {
                let mut placement: WINDOWPLACEMENT = std::mem::zeroed();
                GetWindowPlacement(hwnd, &mut placement);

                self.saved_placement.replace(placement);
                promise::spawn::spawn(async move {
                    let mut mi: MONITORINFO = std::mem::zeroed();
                    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                    GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY), &mut mi);
                    SetWindowLongW(hwnd, GWL_STYLE, style & !(WS_OVERLAPPEDWINDOW as i32));
                    SetWindowPos(
                        hwnd,
                        HWND_TOP,
                        mi.rcMonitor.left,
                        mi.rcMonitor.top,
                        mi.rcMonitor.right - mi.rcMonitor.left,
                        mi.rcMonitor.bottom - mi.rcMonitor.top,
                        SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
                    );
                })
                .detach();
            }
        }
    }
}

impl HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<DisplayHandle, HandleError> {
        unsafe {
            Ok(DisplayHandle::borrow_raw(RawDisplayHandle::Windows(
                WindowsDisplayHandle::new(),
            )))
        }
    }
}

impl HasWindowHandle for Window {
    fn window_handle(&self) -> Result<WindowHandle, HandleError> {
        let conn = Connection::get().expect("raw_window_handle only callable on main thread");
        let handle = conn.get_window(self.0).expect("window handle invalid!?");

        let inner = handle.borrow();
        let handle = inner.window_handle()?;
        unsafe { Ok(WindowHandle::borrow_raw(handle.as_raw())) }
    }
}

#[async_trait(?Send)]
impl WindowOps for Window {
    async fn enable_opengl(&self) -> anyhow::Result<Rc<glium::backend::Context>> {
        let window = self.0;
        promise::spawn::spawn(async move {
            if let Some(handle) = Connection::get().unwrap().get_window(window) {
                let mut inner = handle.borrow_mut();
                inner.enable_opengl()
            } else {
                anyhow::bail!("invalid window");
            }
        })
        .await
    }

    fn notify<T: Any + Send + Sync>(&self, t: T)
    where
        Self: Sized,
    {
        Connection::with_window_inner(self.0, move |inner| {
            inner
                .events
                .dispatch(WindowEvent::Notification(Box::new(t)));
            Ok(())
        });
    }

    fn close(&self) {
        Connection::with_window_inner(self.0, |inner| {
            inner.close();
            Ok(())
        });
    }

    fn show(&self) {
        schedule_show_window(self.0, ShowWindowCommand::Normal);
    }

    fn hide(&self) {
        schedule_show_window(self.0, ShowWindowCommand::Minimize);
    }

    fn focus(&self) {
        let window = self.0;
        let handle = window.0;
        promise::spawn::spawn(async move {
            // In some situation, calling SetForegroundWindow could not bring up the window,
            // This is a little hack which can "steal" the foreground window permission
            // We only call this function in the window creation, so it should be fine.
            // See : https://stackoverflow.com/questions/10740346/setforegroundwindow-only-working-while-visual-studio-is-open
            unsafe {
                let alt_sc = MapVirtualKeyW(VK_MENU as u32, MAPVK_VK_TO_VSC);

                let mut inputs: [INPUT; 2] = [
                    INPUT {
                        type_: INPUT_KEYBOARD,
                        u: Default::default(),
                    },
                    INPUT {
                        type_: INPUT_KEYBOARD,
                        u: Default::default(),
                    },
                ];
                *inputs[0].u.ki_mut() = KEYBDINPUT {
                    wVk: VK_LMENU as u16,
                    wScan: alt_sc as u16,
                    dwFlags: KEYEVENTF_EXTENDEDKEY,
                    dwExtraInfo: 0,
                    time: 0,
                };
                *inputs[1].u.ki_mut() = KEYBDINPUT {
                    wVk: VK_LMENU as u16,
                    wScan: alt_sc as u16,
                    dwFlags: KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP,
                    dwExtraInfo: 0,
                    time: 0,
                };

                // Simulate a key press and release
                SendInput(
                    inputs.len() as u32,
                    inputs.as_mut_ptr(),
                    std::mem::size_of::<INPUT>() as i32,
                );

                SetForegroundWindow(handle);
            }
        })
        .detach();
    }

    fn set_frame_rect(&self, rect: ScreenRect) {
        let hwnd = self.0;
        // Scheduled rather than immediate for the same reason the other window
        // ops here are: SetWindowPos re-enters the window proc, which wants a
        // borrow of the inner state the caller may still be holding.
        promise::spawn::spawn(async move {
            unsafe {
                SetWindowPos(
                    hwnd.0,
                    null_mut(),
                    rect.origin.x as i32,
                    rect.origin.y as i32,
                    rect.size.width as i32,
                    rect.size.height as i32,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        })
        .detach();
    }

    fn maximize(&self) {
        schedule_show_window(self.0, ShowWindowCommand::Maximize);
    }

    fn restore(&self) {
        schedule_show_window(self.0, ShowWindowCommand::Normal);
    }

    fn set_cursor(&self, cursor: Option<MouseCursor>) {
        Connection::with_window_inner(self.0, move |inner| {
            inner.set_cursor(cursor);
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
        let hwnd = self.0 .0;
        promise::spawn::spawn(async move {
            let path = unsafe {
                match pick_folder_dialog(hwnd, &options) {
                    Ok(path) => path,
                    Err(err) => {
                        log::warn!("failed to show folder picker: {err:#}");
                        None
                    }
                }
            };
            callback(path);
        })
        .detach();
    }

    fn pick_app_async(&self, callback: Box<dyn FnOnce(Option<PathBuf>) + 'static>) {
        let hwnd = self.0 .0;
        promise::spawn::spawn(async move {
            let path = unsafe {
                match pick_app_dialog(hwnd) {
                    Ok(path) => path,
                    Err(err) => {
                        log::warn!("failed to show app picker: {err:#}");
                        None
                    }
                }
            };
            callback(path);
        })
        .detach();
    }

    fn invalidate(&self) {
        let hwnd = self.0 .0;
        log::trace!("WindowOps::invalidate calling InvalidateRect");
        unsafe {
            InvalidateRect(hwnd, null(), 0);
        }
    }

    fn set_title(&self, title: &str) {
        let title = title.to_owned();
        Connection::with_window_inner(self.0, move |inner| {
            inner.set_title(&title);
            Ok(())
        });
    }

    fn toggle_fullscreen(&self) {
        Connection::with_window_inner(self.0, move |inner| {
            inner.toggle_fullscreen();
            Ok(())
        });
    }

    fn config_did_change(&self, config: &ConfigHandle) {
        let config = config.clone();
        Connection::with_window_inner(self.0, move |inner| {
            inner.config_did_change(&config);
            Ok(())
        });
    }

    fn set_text_cursor_position(&self, cursor: Rect) {
        Connection::with_window_inner(self.0, move |inner| {
            inner.set_text_cursor_position(cursor);
            Ok(())
        });
    }

    fn set_inner_size(&self, width: usize, height: usize) {
        Connection::with_window_inner(self.0, move |inner| {
            let hwnd = inner.hwnd;
            let decorations = inner.config.window_decorations;
            promise::spawn::spawn(async move {
                log::trace!("set_inner_size called with {width}x{height}");
                let frame_dpi = unsafe { GetDpiForWindow(hwnd.0) };
                let (width, height) = adjust_client_to_window_dimensions(
                    decorations_to_style(decorations),
                    width,
                    height,
                    frame_dpi,
                );
                let window_state = get_window_state(hwnd.0);
                if window_state.can_resize() {
                    log::trace!("set_inner_size now calling SetWindowPos with {width}x{height}");
                    unsafe {
                        SetWindowPos(
                            hwnd.0,
                            hwnd.0,
                            0,
                            0,
                            width,
                            height,
                            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOZORDER,
                        );
                        wm_paint(hwnd.0, 0, 0, 0);
                        if let Some(inner) = rc_from_hwnd(hwnd.0) {
                            let mut inner = inner.borrow_mut();
                            inner.events.dispatch(WindowEvent::SetInnerSizeCompleted);
                        }
                    }
                } else {
                    log::trace!(
                        "ignoring set_inner_size({width}, {height}) call \
                                because window_state is {window_state:?}"
                    );
                }
            })
            .detach();
            Ok(())
        });
    }

    fn set_maximize_button_position(&self, coords: ScreenRect) {
        Connection::with_window_inner(self.0, move |inner| {
            inner.maximize_button_position = Some(coords);
            Ok(())
        });
    }

    fn set_window_position(&self, coords: ScreenPoint) {
        Connection::with_window_inner(self.0, move |inner| {
            inner.set_window_position(coords);
            Ok(())
        });
    }

    fn get_clipboard(&self, _clipboard: Clipboard) -> Future<String> {
        Future::result(
            clipboard_win::get_clipboard_string()
                .map(|s| s.replace("\r\n", "\n"))
                .map_err(|e| anyhow!("Error getting clipboard: {e}")),
        )
    }

    fn get_clipboard_contents(&self, _clipboard: Clipboard) -> Future<ClipboardContents> {
        // Files > text > image: files often travel with a text rendition
        // and are the intent; spreadsheets offer text plus a bitmap of the
        // cells and the text is the intent; screenshots are image-only.
        fn read_contents() -> anyhow::Result<ClipboardContents> {
            use clipboard_win::Getter;

            // Hold the clipboard open across format negotiation so its owner
            // cannot change between the files, text, and bitmap probes.
            let _clipboard = clipboard_win::Clipboard::new_attempts(10)
                .map_err(|err| anyhow!("Error opening clipboard: {err}"))?;

            let mut files = Vec::<String>::new();
            if clipboard_win::formats::FileList
                .read_clipboard(&mut files)
                .is_ok()
                && !files.is_empty()
            {
                return Ok(ClipboardContents::FilePaths(
                    files.into_iter().map(PathBuf::from).collect(),
                ));
            }

            let mut text = String::new();
            if clipboard_win::formats::Unicode
                .read_clipboard(&mut text)
                .is_ok()
            {
                return Ok(ClipboardContents::Text(text.replace("\r\n", "\n")));
            }

            let mut bytes = Vec::new();
            if clipboard_win::formats::Bitmap
                .read_clipboard(&mut bytes)
                .is_ok()
            {
                return Ok(ClipboardContents::Image {
                    format: crate::ClipboardImageFormat::Bmp,
                    bytes,
                });
            }

            Err(anyhow!("Clipboard has no supported files, text, or bitmap"))
        }
        Future::result(read_contents())
    }

    fn set_clipboard(&self, _clipboard: Clipboard, text: String) {
        clipboard_win::set_clipboard_string(&text).ok();
    }

    fn set_window_drag_position(&self, coords: ScreenPoint) {
        Connection::with_window_inner(self.0, move |inner| {
            inner.window_drag_position = Some(coords);

            Ok(())
        });
    }

    fn get_os_parameters(
        &self,
        config: &ConfigHandle,
        window_state: WindowState,
    ) -> anyhow::Result<Option<Parameters>> {
        let hwnd = self.0 .0;
        anyhow::ensure!(!hwnd.is_null(), "HWND is null");

        let has_focus = unsafe { GetFocus() } == hwnd;
        let is_full_screen = window_state.contains(WindowState::FULL_SCREEN);

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let use_accent = hkcu
            .open_subkey("SOFTWARE\\Microsoft\\Windows\\DWM")?
            .get_value::<u32, _>("ColorPrevalence")?;
        let settings = UISettings::new()?;
        let top_border_color = if has_focus {
            if use_accent == 1 {
                wuicolor_to_linearrgba(settings.GetColorValue(UIColorType::Accent)?)
            } else {
                if *IS_WIN10 {
                    LinearRgba(0.01, 0.01, 0.01, 0.67)
                } else {
                    LinearRgba(0.026, 0.026, 0.026, 0.5)
                }
            }
        } else {
            if *IS_WIN10 {
                LinearRgba(0.024, 0.024, 0.024, 0.5)
            } else {
                LinearRgba(0.028, 0.028, 0.028, 0.5)
            }
        };

        const BASE_BORDER: ULength = ULength::new(0);
        let is_resize = config.window_decorations == WindowDecorations::RESIZE;

        Ok(Some(Parameters {
            title_bar: parameters::TitleBar {
                padding_left: ULength::new(0),
                padding_right: ULength::new(0),
                height: None,
                // Nothing reads this: the only consumer of `Parameters` is the
                // GUI's border drawing, which uses `border_dimensions` alone.
                // Populating it meant copying the whole system caption font
                // file into the heap with GetFontData and FreeType-parsing it
                // on every theme change, for a value that was then dropped.
                // macOS has always passed None here for the same reason.
                font_and_size: None,
            },
            border_dimensions: Some(parameters::Border {
                top: if is_resize && !*IS_WIN10 && !is_full_screen {
                    BASE_BORDER + ULength::new(1)
                } else {
                    BASE_BORDER
                },
                left: BASE_BORDER,
                bottom: if is_resize && *IS_WIN10 && !is_full_screen {
                    BASE_BORDER + ULength::new(2)
                } else {
                    BASE_BORDER
                },
                right: BASE_BORDER,
                color: top_border_color,
            }),
        }))
    }
}

/// Set up bidirectional pointers:
/// hwnd.USERDATA -> WindowInner
/// WindowInner.hwnd -> hwnd
unsafe fn wm_nccreate(hwnd: HWND, _msg: UINT, _wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    let create: &CREATESTRUCTW = &*(lparam as *const CREATESTRUCTW);
    let inner = rc_from_pointer(create.lpCreateParams);
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as _);
    inner.borrow_mut().hwnd = HWindow(hwnd);

    None
}

/// Called when the window is being destroyed.
/// Goal is to release the WindowInner reference that was stashed
/// in the window by wm_nccreate.
unsafe fn wm_ncdestroy(
    hwnd: HWND,
    _msg: UINT,
    _wparam: WPARAM,
    _lparam: LPARAM,
) -> Option<LRESULT> {
    let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as LPVOID;
    if !raw.is_null() {
        let inner = take_rc_from_pointer(raw);
        let mut inner = inner.borrow_mut();
        inner.events.dispatch(WindowEvent::Destroyed);
        inner.hwnd = HWindow(null_mut());
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }

    None
}

fn no_native_title_bar(decorations: WindowDecorations) -> bool {
    decorations == WindowDecorations::RESIZE
        || decorations.contains(WindowDecorations::INTEGRATED_BUTTONS)
}

unsafe fn wm_nccalcsize(hwnd: HWND, _msg: UINT, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    let inner = rc_from_hwnd(hwnd)?;
    let inner = match inner.try_borrow() {
        Ok(inner) => inner,
        Err(_) => {
            // We've been called recursively and the upper levels
            // own the borrow. Just take the default action
            return None;
        }
    };

    let no_native_title_bar = no_native_title_bar(inner.config.window_decorations);

    if !(wparam == 1 && no_native_title_bar) {
        return None;
    }

    if inner.saved_placement.is_none() {
        let dpi = inner.get_effective_dpi() as u32;
        let frame_x = GetSystemMetricsForDpi(SM_CXFRAME, dpi);
        let frame_y = GetSystemMetricsForDpi(SM_CYFRAME, dpi);
        let padding = GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);

        let params = (lparam as *mut NCCALCSIZE_PARAMS).as_mut().unwrap();

        let requested_client_rect = &mut params.rgrc[0];

        requested_client_rect.right -= frame_x + padding;
        requested_client_rect.left += frame_x + padding;

        let is_maximized = get_window_state(hwnd) == WindowState::MAXIMIZED;

        // Handle bugged top window border on Windows 10
        if *IS_WIN10 {
            if is_maximized {
                requested_client_rect.top += frame_y + padding;
                requested_client_rect.bottom -= frame_y + padding - 2;
            } else {
                requested_client_rect.top += 1;
                requested_client_rect.bottom -= frame_y - padding;
            }
        } else {
            requested_client_rect.bottom -= frame_y + padding;

            if is_maximized {
                requested_client_rect.top += frame_y + padding;
            }
        }
    }

    Some(0)
}

unsafe fn wm_nchittest(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    let inner = rc_from_hwnd(hwnd)?;
    let inner = match inner.try_borrow() {
        Ok(inner) => inner,
        Err(_) => {
            // We've been called recursively and the upper levels
            // own the borrow. Just take the default action
            return None;
        }
    };

    let no_native_title_bar = no_native_title_bar(inner.config.window_decorations);
    if !no_native_title_bar {
        return None;
    }

    // Let the default procedure handle resizing areas
    let result = DefWindowProcW(hwnd, msg, wparam, lparam);

    if matches!(
        result,
        HTNOWHERE
            | HTRIGHT
            | HTLEFT
            | HTTOPLEFT
            | HTTOP
            | HTTOPRIGHT
            | HTBOTTOMRIGHT
            | HTBOTTOM
            | HTBOTTOMLEFT
    ) {
        return Some(result);
    }

    // The adjustment in NCCALCSIZE messes with the detection
    // of the top hit area so manually fixing that.
    let dpi = inner.get_effective_dpi() as u32;
    let frame_x = GetSystemMetricsForDpi(SM_CXFRAME, dpi) as isize;
    let frame_y = GetSystemMetricsForDpi(SM_CYFRAME, dpi) as isize;
    let padding = GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi) as isize;

    let coords = mouse_coords(lparam);
    let screen_point = ScreenPoint::new(coords.x, coords.y);
    let cursor_point = screen_to_client(hwnd, screen_point);
    let is_maximized = get_window_state(hwnd) == WindowState::MAXIMIZED;

    // check if mouse is in any of the resize areas (HTTOP, HTBOTTOM, etc)

    let mut client_rect = RECT::default();
    let client_rect_is_valid =
        GetClientRect(hwnd, &mut client_rect) == winapi::shared::minwindef::TRUE;

    // Since we are eating the bottom window frame to deal with a Windows 10 bug,
    // we detect resizing in the window client area as a workaround
    if !is_maximized
        && *IS_WIN10
        && client_rect_is_valid
        && cursor_point.y >= (client_rect.bottom as isize) - (frame_y + padding)
    {
        if cursor_point.x <= (frame_x + padding) {
            return Some(HTBOTTOMLEFT);
        } else if cursor_point.x >= (client_rect.right as isize) - (frame_x + padding) {
            return Some(HTBOTTOMRIGHT);
        } else {
            return Some(HTBOTTOM);
        }
    }

    if !is_maximized && cursor_point.y >= 0 && cursor_point.y < frame_y {
        if cursor_point.x <= (frame_x + padding) {
            return Some(HTTOPLEFT);
        } else if cursor_point.x >= (client_rect.right as isize) - (frame_x + padding) {
            return Some(HTTOPRIGHT);
        } else {
            return Some(HTTOP);
        }
    }

    if let Some(coords) = inner.window_drag_position {
        if coords == screen_point && inner.saved_placement.is_none() {
            return Some(HTCAPTION);
        }
    }

    let use_snap_layouts = !*IS_WIN10;
    if use_snap_layouts {
        if let Some(max) = inner.maximize_button_position {
            if max.contains(screen_point) {
                return Some(HTMAXBUTTON);
            }
        }
    }

    Some(HTCLIENT)
}

fn get_window_state(hwnd: HWND) -> WindowState {
    let mut placement = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as _,
        ..Default::default()
    };

    let placement =
        if unsafe { GetWindowPlacement(hwnd, &mut placement) } == winapi::shared::minwindef::TRUE {
            placement.showCmd as i32
        } else {
            0
        };

    match placement {
        SW_SHOWMAXIMIZED => WindowState::MAXIMIZED,
        SW_SHOWMINIMIZED => WindowState::HIDDEN,
        _ => unsafe {
            let mut rect = std::mem::zeroed();
            GetWindowRect(hwnd, &mut rect);

            let mut mi: MONITORINFO = std::mem::zeroed();
            mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi);

            if mi.rcMonitor.left == rect.left
                && mi.rcMonitor.top == rect.top
                && mi.rcMonitor.right == rect.right
                && mi.rcMonitor.bottom == rect.bottom
            {
                WindowState::FULL_SCREEN
            } else {
                WindowState::default()
            }
        },
    }
}

/// "Blur behind" is the old vista term for a cool blurring
/// effect that the DWM could enable.  Subsequent windows
/// versions have removed the blurring.  We use this call
/// to tell DWM that we set proper alpha channel info as
/// a result of rendering our window content.
fn enable_blur_behind(hwnd: HWND) {
    use winapi::shared::minwindef::*;
    use winapi::um::dwmapi::*;
    use winapi::um::wingdi::*;

    unsafe {
        let region = CreateRectRgn(0, 0, -1, -1);

        let bb = DWM_BLURBEHIND {
            dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
            fEnable: TRUE,
            hRgnBlur: region,
            fTransitionOnMaximized: FALSE,
        };

        DwmEnableBlurBehindWindow(hwnd, &bb);

        DeleteObject(region as _);
    }
}

fn apply_theme(hwnd: HWND) -> Option<LRESULT> {
    // Check for OS app theme, and set window attributes accordingly.
    // Note that the MS terminal app uses the logic found here for this stuff:
    // https://github.com/microsoft/terminal/blob/9b92986b49bed8cc41fde4d6ef080921c41e6d9e/src/interactivity/win32/windowtheme.cpp#L62
    use winapi::um::dwmapi::{DwmExtendFrameIntoClientArea, DwmSetWindowAttribute};
    use winapi::um::uxtheme::MARGINS;

    #[allow(non_snake_case)]
    type WINDOWCOMPOSITIONATTRIB = u32;
    const WCA_USEDARKMODECOLORS: WINDOWCOMPOSITIONATTRIB = 26;

    #[allow(non_snake_case)]
    #[repr(C)]
    pub struct WINDOWCOMPOSITIONATTRIBDATA {
        Attrib: WINDOWCOMPOSITIONATTRIB,
        pvData: PVOID,
        cbData: winapi::shared::basetsd::SIZE_T,
    }

    shared_library!(User32,
        pub fn SetWindowCompositionAttribute(hwnd: HWND, attrib: *mut WINDOWCOMPOSITIONATTRIBDATA) -> BOOL,
    );

    const DWMWA_USE_IMMERSIVE_DARK_MODE: DWORD = 20;
    const DWMWA_MICA_EFFECT: DWORD = 1029;
    const DWMWA_SYSTEMBACKDROP_TYPE: DWORD = 38;

    #[allow(non_camel_case_types)]
    #[allow(dead_code)]
    #[derive(PartialEq, Eq)]
    #[repr(C)]
    enum ACCENT_STATE {
        ACCENT_DISABLED = 0,
        ACCENT_ENABLE_BLURBEHIND = 3,
        ACCENT_ENABLE_ACRYLICBLURBEHIND = 4,
    }

    #[allow(non_snake_case)]
    #[repr(C)]
    struct ACCENT_POLICY {
        AccentState: u32,
        AccentFlags: u32,
        GradientColour: u32,
        AnimationId: u32,
    }

    #[allow(non_camel_case_types)]
    #[allow(dead_code)]
    #[repr(C)]
    enum DWM_SYSTEMBACKDROP_TYPE {
        DWMSBT_AUTO = 0,
        DWMSBT_NONE = 1,
        DWMSBT_MAINWINDOW = 2,      // Mica
        DWMSBT_TRANSIENTWINDOW = 3, // Acrylic
        DWMSBT_TABBEDWINDOW = 4,    // Tabbed
    }

    unsafe {
        let appearance = get_appearance();
        let theme_string = if appearance == Appearance::Dark {
            "DarkMode_Explorer"
        } else {
            ""
        };

        SetWindowTheme(
            hwnd as _,
            wide_string(theme_string).as_slice().as_ptr(),
            std::ptr::null_mut(),
        );

        let mut enabled: BOOL = if appearance == Appearance::Dark { 1 } else { 0 };
        DwmSetWindowAttribute(
            hwnd as _,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &enabled as *const _ as *const _,
            std::mem::size_of_val(&enabled) as u32,
        );

        if let Ok(user) = User32::open(std::path::Path::new("user32.dll")) {
            (user.SetWindowCompositionAttribute)(
                hwnd,
                &mut WINDOWCOMPOSITIONATTRIBDATA {
                    Attrib: WCA_USEDARKMODECOLORS,
                    pvData: &mut enabled as *mut _ as _,
                    cbData: std::mem::size_of_val(&enabled) as _,
                },
            );
        };

        if let Some(inner) = rc_from_hwnd(hwnd) {
            let mut inner = inner.borrow_mut();

            // Set Acrylic or Mica system Backdrop
            let pv_attribute = match inner.config.win32_system_backdrop {
                SystemBackdrop::Auto => DWM_SYSTEMBACKDROP_TYPE::DWMSBT_AUTO,
                SystemBackdrop::Disable => DWM_SYSTEMBACKDROP_TYPE::DWMSBT_NONE,
                SystemBackdrop::Acrylic => DWM_SYSTEMBACKDROP_TYPE::DWMSBT_TRANSIENTWINDOW,
                SystemBackdrop::Mica => DWM_SYSTEMBACKDROP_TYPE::DWMSBT_MAINWINDOW,
                SystemBackdrop::Tabbed => DWM_SYSTEMBACKDROP_TYPE::DWMSBT_TABBEDWINDOW,
            };

            let margins = match inner.config.window_decorations {
                WindowDecorations::TITLE => -1,
                _ => 0,
            };

            DwmExtendFrameIntoClientArea(
                hwnd,
                &MARGINS {
                    cxLeftWidth: margins,
                    cxRightWidth: margins,
                    cyTopHeight: margins,
                    cyBottomHeight: margins,
                },
            );

            // Apply Acrylic or Mica Backdrop
            if *IS_WIN11_22H2 {
                DwmSetWindowAttribute(
                    hwnd,
                    DWMWA_SYSTEMBACKDROP_TYPE,
                    &pv_attribute as *const _ as _,
                    std::mem::size_of_val(&pv_attribute) as u32,
                );
            } else {
                let mut colour = inner.config.win32_acrylic_accent_color.to_srgb_u8();
                colour.3 = if colour.3 == 0 { 1 } else { colour.3 }; // acrylic doesn't like to have 0 alpha

                let mut policy = ACCENT_POLICY {
                    AccentState: if inner.config.win32_system_backdrop == SystemBackdrop::Acrylic {
                        ACCENT_STATE::ACCENT_ENABLE_ACRYLICBLURBEHIND as _
                    } else {
                        ACCENT_STATE::ACCENT_DISABLED as _
                    },
                    AccentFlags: if inner.config.win32_system_backdrop == SystemBackdrop::Acrylic {
                        2
                    } else {
                        0
                    },
                    GradientColour: (colour.0 as u32)
                        | (colour.1 as u32) << 8
                        | (colour.2 as u32) << 16
                        | (colour.3 as u32) << 24,
                    AnimationId: 0,
                };

                if let Ok(user) = User32::open(std::path::Path::new("user32.dll")) {
                    (user.SetWindowCompositionAttribute)(
                        hwnd,
                        &mut WINDOWCOMPOSITIONATTRIBDATA {
                            Attrib: 0x13,
                            pvData: &mut policy as *mut _ as _,
                            cbData: std::mem::size_of_val(&policy) as _,
                        },
                    );
                }

                if !*IS_WIN10 && !*IS_WIN11_22H2 {
                    // For build versions less than 22h2 but are still win11
                    let mica_enabled: u32 =
                        if inner.config.win32_system_backdrop == SystemBackdrop::Mica {
                            1
                        } else {
                            0
                        };
                    DwmSetWindowAttribute(
                        hwnd,
                        DWMWA_MICA_EFFECT,
                        &mica_enabled as *const _ as _,
                        std::mem::size_of_val(&mica_enabled) as u32,
                    );
                }
            }

            if appearance != inner.appearance {
                inner.appearance = appearance;
                inner
                    .events
                    .dispatch(WindowEvent::AppearanceChanged(appearance));
            }
        }
    }

    None
}

unsafe fn wm_enter_exit_size_move(
    hwnd: HWND,
    msg: UINT,
    _wparam: WPARAM,
    _lparam: LPARAM,
) -> Option<LRESULT> {
    let mut should_size = false;
    if let Some(inner) = rc_from_hwnd(hwnd) {
        let mut inner = inner.borrow_mut();
        inner.in_size_move = msg == WM_ENTERSIZEMOVE;
        should_size = !inner.in_size_move;
    }

    if should_size {
        wm_size(hwnd, 0, 0, 0)?;
    }

    Some(0)
}

/// Asked just before `WM_DPICHANGED`: `wparam` is the dpi the window is moving
/// to and `lparam` points at a `SIZE` holding the window size Windows intends
/// to use, which we may overwrite. Returning FALSE keeps Windows' own scaling.
///
/// This is the supported place to ask for a different size, and it is what
/// lets `wm_dpi_changed` accept every suggested rectangle unconditionally.
///
/// The one case where Windows' default is wrong for us is an ordinary
/// floating window whose `dpi` is pinned globally: its layout deliberately
/// ignores the monitor, so the client area has to keep its pixel size while
/// the frame around it follows the new scale. Everything else declines:
///
/// - Per-monitor `dpi_by_screen` entries cannot be answered here, because the
///   message carries a dpi but not the monitor it belongs to. Holding the size
///   would override a per-screen value we are not able to look up, so these
///   fall through to Windows' scaling and are settled by the resize that
///   follows.
/// - Maximized, minimized and our own full-screen windows do not derive their
///   size from the client area we are holding on to. Answering for them would
///   pin the *old* monitor's dimensions over the ones Windows computed for the
///   new one.
unsafe fn wm_get_dpi_scaled_size(
    hwnd: HWND,
    _msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    let inner = rc_from_hwnd(hwnd)?;
    let (globally_pinned, has_per_screen, self_drawn_full_screen) = {
        let inner = inner.borrow();
        (
            inner.config.dpi.is_some(),
            !inner.config.dpi_by_screen.is_empty(),
            inner.saved_placement.is_some(),
        )
    };

    // FALSE anywhere below: scale us the way you were going to.
    if !globally_pinned || has_per_screen || self_drawn_full_screen {
        return Some(0);
    }
    if get_window_state(hwnd)
        .intersects(WindowState::MAXIMIZED | WindowState::FULL_SCREEN | WindowState::HIDDEN)
    {
        return Some(0);
    }

    let new_dpi = wparam as u32;
    let size = lparam as *mut SIZE;
    if size.is_null() || new_dpi == 0 {
        return Some(0);
    }

    let mut client: RECT = std::mem::zeroed();
    if GetClientRect(hwnd, &mut client) == 0 {
        return Some(0);
    }

    let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
    let (width, height) = adjust_client_to_window_dimensions(
        style,
        rect_width(&client).max(0) as usize,
        rect_height(&client).max(0) as usize,
        new_dpi,
    );

    log::trace!("WM_GETDPISCALEDSIZE: dpi is pinned, holding client size at {new_dpi}dpi -> window {width}x{height}");

    (*size).cx = width;
    (*size).cy = height;
    // TRUE: use the size we just wrote.
    Some(1)
}

/// Windows sends this when the window lands on a monitor with a different
/// scale factor, and hands us in `lparam` the rectangle it wants us to occupy
/// there.
///
/// Apply it, always. We are manifested `PerMonitorV2`, which makes scaling a
/// top-level window the application's job, and the message is documented as
/// "you will need to resize and reposition your window based on the
/// suggestions provided by lParam and using SetWindowPos. If you do not do
/// this, your window will grow or shrink with respect to everything else on
/// the new monitor."
///
/// There is no exemption here for maximized or pinned-dpi windows: Windows
/// tracks maximized state itself and accounts for it in the rectangle, and a
/// window that wants a different size says so in `wm_get_dpi_scaled_size`,
/// which is asked first and whose answer becomes the candidate rectangle we
/// get here.
///
/// Our *own* full-screen mode is the exception, because Windows cannot account
/// for what it does not know about: `toggle_fullscreen` strips the frame off
/// the window and covers the monitor by hand, so as far as Windows is
/// concerned this is an ordinary borderless window and it scales the old
/// rectangle linearly like any other. That lands near, but not on, the new
/// monitor's bounds - leaving a full-screen window with a seam or overhanging
/// an edge. Take the monitor the suggestion points at and use its bounds
/// instead, exactly as `toggle_fullscreen` does. `saved_placement` is left
/// untouched so leaving full screen still restores the original geometry.
///
/// We only move and size. The `WM_WINDOWPOSCHANGED` that our `SetWindowPos`
/// triggers is what refreshes the dpi, fonts and gpu surface, and
/// `check_and_call_resize_if_needed` compares against `last_size` before
/// dispatching, so a suggestion that changes nothing costs one comparison
/// rather than a second resize.
unsafe fn wm_dpi_changed(
    hwnd: HWND,
    _msg: UINT,
    _wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    let suggested = lparam as *const RECT;
    if suggested.is_null() {
        return Some(0);
    }
    let suggested = *suggested;

    let self_drawn_full_screen =
        rc_from_hwnd(hwnd).map_or(false, |inner| inner.borrow().saved_placement.is_some());

    let target = if self_drawn_full_screen {
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let mon = MonitorFromRect(&suggested, MONITOR_DEFAULTTONEAREST);
        if GetMonitorInfoW(mon, &mut mi) != 0 {
            mi.rcMonitor
        } else {
            suggested
        }
    } else {
        suggested
    };

    let width = rect_width(&target);
    let height = rect_height(&target);
    if width <= 0 || height <= 0 {
        return Some(0);
    }

    log::trace!(
        "WM_DPICHANGED: taking {} rect {}x{} at {},{}",
        if self_drawn_full_screen {
            "target monitor"
        } else {
            "suggested"
        },
        width,
        height,
        target.left,
        target.top
    );

    SetWindowPos(
        hwnd,
        null_mut(),
        target.left,
        target.top,
        width,
        height,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );

    Some(0)
}

/// We handle WM_WINDOWPOSCHANGED and dispatch directly to our wm_size as it
/// is a bit more efficient than letting DefWindowProcW parse this and
/// trigger WM_SIZE.
unsafe fn wm_windowposchanged(
    hwnd: HWND,
    _msg: UINT,
    _wparam: WPARAM,
    _lparam: LPARAM,
) -> Option<LRESULT> {
    // let pos = &*(lparam as *const WINDOWPOS);
    dispatch_occlusion_changed_if_needed(hwnd);
    dispatch_frame_changed_if_needed(hwnd);
    wm_size(hwnd, 0, 0, 0)?;
    Some(0)
}

/// The restore geometry of the window, or `None` if this is a moment whose
/// geometry is not worth remembering.
///
/// A minimized window has no meaningful rect at all. A full-screen one -- ours
/// or the system's idea of one -- has the monitor's, which is not something a
/// later launch should reopen at. A maximized window has the work area's, so
/// its *restore* rect is read from `GetWindowPlacement` instead: that is the
/// rect Windows itself would put the window back at.
///
/// `rcNormalPosition` is documented as being in workspace coordinates, which
/// differ from screen coordinates when the taskbar is docked to the top or the
/// left edge, so it is shifted by the monitor's work-area origin to match what
/// `GetWindowRect` would have said. The ordinary, unmaximized path takes
/// `GetWindowRect` directly and needs no such correction.
unsafe fn restore_frame_rect(
    hwnd: HWND,
    self_drawn_full_screen: bool,
) -> Option<(ScreenRect, bool)> {
    if IsIconic(hwnd) != winapi::shared::minwindef::FALSE {
        return None;
    }

    let state = get_window_state(hwnd);
    if self_drawn_full_screen || state == WindowState::FULL_SCREEN {
        return None;
    }

    let maximized = state == WindowState::MAXIMIZED;

    let rect = if maximized {
        let mut placement = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as _,
            ..Default::default()
        };
        if GetWindowPlacement(hwnd, &mut placement) != winapi::shared::minwindef::TRUE {
            return None;
        }

        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let (dx, dy) =
            if GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi) != 0 {
                (
                    mi.rcWork.left - mi.rcMonitor.left,
                    mi.rcWork.top - mi.rcMonitor.top,
                )
            } else {
                (0, 0)
            };

        let mut rect = placement.rcNormalPosition;
        rect.left += dx;
        rect.right += dx;
        rect.top += dy;
        rect.bottom += dy;
        rect
    } else {
        let mut rect: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut rect) == 0 {
            return None;
        }
        rect
    };

    let width = rect_width(&rect);
    let height = rect_height(&rect);
    if width <= 0 || height <= 0 {
        return None;
    }

    Some((
        euclid::rect(
            rect.left as isize,
            rect.top as isize,
            width as isize,
            height as isize,
        ),
        maximized,
    ))
}

/// Tell the owner where the window would want to reopen. WM_WINDOWPOSCHANGED
/// is the only message that covers plain moves as well as resizes, and it is
/// noisy, so -- as with the occlusion dispatch above -- only edges go out.
unsafe fn dispatch_frame_changed_if_needed(hwnd: HWND) {
    let Some(inner) = rc_from_hwnd(hwnd) else {
        return;
    };
    let mut inner = inner.borrow_mut();

    let Some(current) = restore_frame_rect(hwnd, inner.saved_placement.is_some()) else {
        return;
    };
    if inner.last_frame_event == Some(current) {
        return;
    }
    inner.last_frame_event = Some(current);

    let (frame, maximized) = current;
    inner
        .events
        .dispatch(WindowEvent::WindowFrameChanged { frame, maximized });
}

fn window_is_effectively_visible(is_visible: bool, is_iconic: bool) -> bool {
    is_visible && !is_iconic
}

/// Windows has no AppKit-style full-occlusion notification, but the two
/// expensive and reliable inactive states are queryable on every window-pos
/// edge: a hidden HWND is not visible and a minimized HWND is iconic.
unsafe fn dispatch_occlusion_changed_if_needed(hwnd: HWND) {
    let visible = window_is_effectively_visible(
        IsWindowVisible(hwnd) != winapi::shared::minwindef::FALSE,
        IsIconic(hwnd) != winapi::shared::minwindef::FALSE,
    );
    let Some(inner) = rc_from_hwnd(hwnd) else {
        return;
    };
    let mut inner = inner.borrow_mut();
    if inner.occlusion_visible == Some(visible) {
        return;
    }
    inner.occlusion_visible = Some(visible);
    inner
        .events
        .dispatch(WindowEvent::OcclusionChanged(visible));
}

unsafe fn wm_size(hwnd: HWND, _msg: UINT, _wparam: WPARAM, _lparam: LPARAM) -> Option<LRESULT> {
    let mut should_paint = false;
    let mut should_pump = false;

    if let Some(inner) = rc_from_hwnd(hwnd) {
        let mut inner = inner.borrow_mut();
        should_paint = inner.check_and_call_resize_if_needed();
        should_pump = inner.in_size_move;
    }

    if should_paint {
        wm_paint(hwnd, 0, 0, 0)?;
        if should_pump {
            crate::spawn::SPAWN_QUEUE.run();
        }
    }

    None
}

unsafe fn wm_set_focus(
    hwnd: HWND,
    _msg: UINT,
    _wparam: WPARAM,
    _lparam: LPARAM,
) -> Option<LRESULT> {
    rc_from_hwnd(hwnd)?
        .borrow_mut()
        .events
        .dispatch(WindowEvent::FocusChanged(true));
    None
}

unsafe fn wm_kill_focus(
    hwnd: HWND,
    _msg: UINT,
    _wparam: WPARAM,
    _lparam: LPARAM,
) -> Option<LRESULT> {
    rc_from_hwnd(hwnd)?
        .borrow_mut()
        .events
        .dispatch(WindowEvent::FocusChanged(false));
    None
}

unsafe fn wm_paint(hwnd: HWND, _msg: UINT, _wparam: WPARAM, _lparam: LPARAM) -> Option<LRESULT> {
    let inner = rc_from_hwnd(hwnd)?;
    let mut inner = inner.borrow_mut();

    if inner.paint_throttled {
        inner.invalidated = true;
        // Returning Some(0) suppresses DefWindowProcW, which is the only other
        // thing that would have validated the update region -- so without this
        // the region stays dirty, PeekMessageW keeps synthesising WM_PAINT and
        // never returns 0, and the message loop never reaches wait_message().
        // That spins a core flat out for the whole throttle window (~8ms at the
        // default max_fps of 120), every window, whenever output is flowing.
        //
        // No frame is lost: `invalidated` is now true, and the timer that
        // clears `paint_throttled` re-issues InvalidateRect for the whole
        // client area in the same closure under the same borrow. Discarding the
        // OS's partial expose region is fine because nothing reads ps.rcPaint;
        // repaints are always full-window.
        ValidateRect(hwnd, null());
        return Some(0);
    }

    let mut ps = PAINTSTRUCT {
        fErase: 0,
        fIncUpdate: 0,
        fRestore: 0,
        hdc: std::ptr::null_mut(),
        rcPaint: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        rgbReserved: [0; 32],
    };
    let _ = BeginPaint(hwnd, &mut ps);
    // Do nothing right now
    EndPaint(hwnd, &mut ps);

    inner.invalidated = false;
    // Ask the app to repaint in a bit
    inner.events.dispatch(WindowEvent::NeedRepaint);

    inner.paint_throttled = true;
    let window_id = inner.hwnd;
    let max_fps = inner.config.max_fps;
    promise::spawn::spawn(async move {
        async_io::Timer::after(std::time::Duration::from_millis(1000 / max_fps as u64)).await;
        Connection::with_window_inner(window_id, move |inner| {
            inner.paint_throttled = false;
            if inner.invalidated {
                InvalidateRect(inner.hwnd.0, null(), 0);
            }
            Ok(())
        });
    })
    .detach();

    Some(0)
}

fn mods_and_buttons(wparam: WPARAM) -> (Modifiers, MouseButtons) {
    let mut modifiers = Modifiers::default();
    let mut buttons = MouseButtons::default();
    if wparam & MK_CONTROL != 0 {
        modifiers |= Modifiers::CTRL;
    }
    if wparam & MK_SHIFT != 0 {
        modifiers |= Modifiers::SHIFT;
    }
    if unsafe { GetKeyState(VK_MENU) } as u16 & 0x8000 != 0 {
        modifiers |= Modifiers::ALT;
    }
    if wparam & MK_LBUTTON != 0 {
        buttons |= MouseButtons::LEFT;
    }
    if wparam & MK_MBUTTON != 0 {
        buttons |= MouseButtons::MIDDLE;
    }
    if wparam & MK_RBUTTON != 0 {
        buttons |= MouseButtons::RIGHT;
    }
    // TODO: XBUTTON1 and XBUTTON2?
    (modifiers, buttons)
}

fn mouse_coords(lparam: LPARAM) -> Point {
    let point = MAKEPOINTS(lparam as _);
    Point::new(point.x as _, point.y as _)
}

fn nc_mouse_coords(hwnd: HWND, lparam: LPARAM) -> Point {
    let point = MAKEPOINTS(lparam as _);
    let point = ScreenPoint::new(point.x as _, point.y as _);
    screen_to_client(hwnd, point)
}

fn screen_to_client(hwnd: HWND, point: ScreenPoint) -> Point {
    let mut point = POINT {
        x: point.x.try_into().unwrap(),
        y: point.y.try_into().unwrap(),
    };
    unsafe { ScreenToClient(hwnd, &mut point as *mut _) };
    Point::new(point.x.try_into().unwrap(), point.y.try_into().unwrap())
}

fn client_to_screen(hwnd: HWND, point: Point) -> ScreenPoint {
    let mut point = POINT {
        x: point.x.try_into().unwrap(),
        y: point.y.try_into().unwrap(),
    };
    unsafe { ClientToScreen(hwnd, &mut point as *mut _) };
    ScreenPoint::new(point.x.try_into().unwrap(), point.y.try_into().unwrap())
}

fn apply_mouse_cursor(cursor: Option<MouseCursor>) {
    match cursor {
        None => unsafe {
            SetCursor(null_mut());
        },
        Some(cursor) => unsafe {
            SetCursor(LoadCursorW(
                null_mut(),
                match cursor {
                    MouseCursor::Arrow => IDC_ARROW,
                    MouseCursor::Hand => IDC_HAND,
                    MouseCursor::Text => IDC_IBEAM,
                    MouseCursor::SizeUpDown => IDC_SIZENS,
                    MouseCursor::SizeLeftRight => IDC_SIZEWE,
                    MouseCursor::SizeNorthWestSouthEast => IDC_SIZENWSE,
                    MouseCursor::SizeNorthEastSouthWest => IDC_SIZENESW,
                },
            ));
        },
    }
}

unsafe fn mouse_button(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    let inner = rc_from_hwnd(hwnd)?;
    // To support dragging the window, capture when the left
    // button goes down and release when it goes up.
    // Without this, the drag state can be confused when dragging
    // the mouse up outside of the client area.
    if msg == WM_LBUTTONDOWN {
        SetCapture(hwnd);
    } else if msg == WM_LBUTTONUP {
        ReleaseCapture();
    }
    let (modifiers, mouse_buttons) = mods_and_buttons(wparam);
    let coords = mouse_coords(lparam);
    let event = MouseEvent {
        kind: match msg {
            WM_LBUTTONDOWN => MouseEventKind::Press(MousePress::Left),
            WM_LBUTTONUP => MouseEventKind::Release(MousePress::Left),
            WM_RBUTTONDOWN => MouseEventKind::Press(MousePress::Right),
            WM_RBUTTONUP => MouseEventKind::Release(MousePress::Right),
            WM_MBUTTONDOWN => MouseEventKind::Press(MousePress::Middle),
            WM_MBUTTONUP => MouseEventKind::Release(MousePress::Middle),
            _ => return None,
        },
        coords,
        screen_coords: client_to_screen(hwnd, coords),
        mouse_buttons,
        modifiers,
        precise_scroll_delta: None,
        scroll_phase: None,
        momentum_phase: None,
    };
    inner
        .borrow_mut()
        .events
        .dispatch(WindowEvent::MouseEvent(event));
    Some(0)
}

unsafe fn nc_mouse_button(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    let inner = rc_from_hwnd(hwnd)?;

    let no_native_title_bar = no_native_title_bar(inner.borrow().config.window_decorations);
    if !no_native_title_bar {
        // Don't mess with this event unless we're doing our own custom
        // titlebar
        return None;
    }

    // To support dragging the window, capture when the left
    // button goes down and release when it goes up.
    // Without this, the drag state can be confused when dragging
    // the mouse up outside of the client area.

    if msg == WM_LBUTTONDOWN {
        SetCapture(hwnd);
    } else if msg == WM_LBUTTONUP {
        ReleaseCapture();
    }

    if wparam != HTMAXBUTTON as usize {
        return None;
    }

    let (modifiers, mouse_buttons) = mods_and_buttons(0);
    let coords = nc_mouse_coords(hwnd, lparam);

    let event = MouseEvent {
        kind: match msg {
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK => MouseEventKind::Press(MousePress::Left),
            _ => return None,
        },
        coords,
        screen_coords: client_to_screen(hwnd, coords),
        mouse_buttons,
        modifiers,
        precise_scroll_delta: None,
        scroll_phase: None,
        momentum_phase: None,
    };
    inner
        .borrow_mut()
        .events
        .dispatch(WindowEvent::MouseEvent(event));
    Some(0)
}

unsafe fn mouse_move(hwnd: HWND, _msg: UINT, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    let inner = rc_from_hwnd(hwnd)?;
    let mut inner = inner.borrow_mut();

    if !inner.track_mouse_leave {
        inner.track_mouse_leave = true;

        let mut trk = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };

        inner.track_mouse_leave = TrackMouseEvent(&mut trk) == winapi::shared::minwindef::TRUE;
    }

    let (modifiers, mouse_buttons) = mods_and_buttons(wparam);
    let coords = mouse_coords(lparam);
    let event = MouseEvent {
        kind: MouseEventKind::Move,
        coords,
        screen_coords: client_to_screen(hwnd, coords),
        mouse_buttons,
        modifiers,
        precise_scroll_delta: None,
        scroll_phase: None,
        momentum_phase: None,
    };

    inner.events.dispatch(WindowEvent::MouseEvent(event));
    Some(0)
}

unsafe fn nc_mouse_move(hwnd: HWND, _msg: UINT, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    let inner = rc_from_hwnd(hwnd)?;
    let mut inner = inner.borrow_mut();

    if !inner.track_mouse_leave {
        inner.track_mouse_leave = true;

        let mut trk = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE | TME_NONCLIENT,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };

        inner.track_mouse_leave = TrackMouseEvent(&mut trk) == winapi::shared::minwindef::TRUE;
    }

    if wparam != HTMAXBUTTON as usize {
        return None;
    }

    let (modifiers, mouse_buttons) = mods_and_buttons(0);
    let coords = nc_mouse_coords(hwnd, lparam);

    let event = MouseEvent {
        kind: MouseEventKind::Move,
        coords,
        screen_coords: client_to_screen(hwnd, coords),
        mouse_buttons,
        modifiers,
        precise_scroll_delta: None,
        scroll_phase: None,
        momentum_phase: None,
    };

    inner.events.dispatch(WindowEvent::MouseEvent(event));
    inner.events.dispatch(WindowEvent::NeedRepaint);

    Some(0)
}

unsafe fn mouse_leave(hwnd: HWND, _msg: UINT, _wparam: WPARAM, _lparam: LPARAM) -> Option<LRESULT> {
    let inner = rc_from_hwnd(hwnd)?;
    let mut inner = inner.borrow_mut();

    inner.track_mouse_leave = false;
    inner.events.dispatch(WindowEvent::MouseLeave);

    Some(0)
}

lazy_static! {
    static ref WHEEL_SCROLL_LINES: i16 = read_scroll_speed("WheelScrollLines").unwrap_or(3);
    static ref WHEEL_SCROLL_CHARS: i16 = read_scroll_speed("WheelScrollChars").unwrap_or(3);
}

fn read_scroll_speed(name: &str) -> io::Result<i16> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let desktop = hkcu.open_subkey("Control Panel\\Desktop")?;
    desktop
        .get_value::<String, _>(name)
        .and_then(|v| v.parse().map_err(|_| io::ErrorKind::InvalidData.into()))
}

unsafe fn mouse_wheel(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    let inner = rc_from_hwnd(hwnd)?;
    let (modifiers, mouse_buttons) = mods_and_buttons(wparam);
    // Wheel events return screen coordinates!
    let coords = mouse_coords(lparam);
    let screen_coords = ScreenPoint::new(coords.x, coords.y);
    let coords = screen_to_client(hwnd, screen_coords);
    let delta = GET_WHEEL_DELTA_WPARAM(wparam);
    let scaled_delta = if msg == WM_MOUSEWHEEL {
        delta * (*WHEEL_SCROLL_LINES)
    } else {
        delta * (*WHEEL_SCROLL_CHARS)
    };
    let mut position = scaled_delta / WHEEL_DELTA;
    let remainder = scaled_delta % WHEEL_DELTA;
    let event = MouseEvent {
        kind: if msg == WM_MOUSEHWHEEL {
            let mut inner = inner.borrow_mut();
            if inner.hscroll_remainder.signum() != remainder.signum() {
                // Reset remainder when changing scroll direction
                inner.hscroll_remainder = 0;
            }
            inner.hscroll_remainder += remainder;
            position += inner.hscroll_remainder / WHEEL_DELTA;
            inner.hscroll_remainder %= WHEEL_DELTA;
            log::trace!(
                "mouse_hwheel delta={} scaled={} remainder={} pos={}",
                delta,
                scaled_delta,
                inner.hscroll_remainder,
                position
            );
            if position == 0 {
                return Some(0);
            }
            MouseEventKind::HorzWheel(position)
        } else {
            let mut inner = inner.borrow_mut();
            if inner.vscroll_remainder.signum() != remainder.signum() {
                // Reset remainder when changing scroll direction
                inner.vscroll_remainder = 0;
            }
            inner.vscroll_remainder += remainder;
            position += inner.vscroll_remainder / WHEEL_DELTA;
            inner.vscroll_remainder %= WHEEL_DELTA;
            log::trace!(
                "mouse_wheel delta={} scaled={} remainder={} pos={}",
                delta,
                scaled_delta,
                inner.vscroll_remainder,
                position
            );
            if position == 0 {
                return Some(0);
            }
            MouseEventKind::VertWheel(position)
        },
        coords,
        screen_coords,
        mouse_buttons,
        modifiers,
        precise_scroll_delta: None,
        scroll_phase: None,
        momentum_phase: None,
    };
    inner
        .borrow_mut()
        .events
        .dispatch(WindowEvent::MouseEvent(event));
    Some(0)
}

/// Helper for managing the IME Manager
struct ImmContext {
    hwnd: HWND,
    imc: HIMC,
}

impl ImmContext {
    /// Obtain the IMM context; it will be released automatically
    /// when dropped
    pub fn get(hwnd: HWND) -> Self {
        Self {
            hwnd,
            imc: unsafe { ImmGetContext(hwnd) },
        }
    }

    /// Set the position of the IME candidate window relative to the cursor.
    pub fn set_candidate_window_position(&self, cursor: Rect) {
        let mut cf = CANDIDATEFORM {
            dwIndex: 0,
            // Don't draw the IME candidate window on the cursor
            // to prevent the window from hiding composition (preedit) string
            dwStyle: CFS_EXCLUDE,
            // cursor position the IME candidate window bases on
            ptCurrentPos: POINT {
                x: cursor.origin.x.max(0) as i32,
                y: cursor.origin.y.max(0) as i32,
            },
            // cursor rectangle the IME candidate window excludes
            rcArea: RECT {
                left: cursor.min_x().max(0) as i32,
                top: cursor.min_y().max(0) as i32,
                right: cursor.max_x().max(0) as i32,
                bottom: cursor.max_y().max(0) as i32,
            },
        };
        unsafe {
            ImmSetCandidateWindow(self.imc, &mut cf);
        }
    }

    /// Set the position of the IME composition window relative to the cursor.
    pub fn set_composition_window_position(&self, cursor: Rect) {
        let mut cf = COMPOSITIONFORM {
            dwStyle: CFS_POINT,
            ptCurrentPos: POINT {
                x: cursor.origin.x.max(0) as i32,
                y: cursor.origin.y.max(0) as i32,
            },
            rcArea: RECT::default(),
        };
        unsafe {
            ImmSetCompositionWindow(self.imc, &mut cf);
        }
    }

    pub fn get_str(&self, which: DWORD) -> Result<String, OsString> {
        // This returns a size in bytes even though it is for a buffer of u16!
        let byte_size =
            unsafe { ImmGetCompositionStringW(self.imc, which, std::ptr::null_mut(), 0) };
        if byte_size > 0 {
            let word_size = byte_size as usize / 2;
            let mut wide_buf = vec![0u16; word_size];
            unsafe {
                ImmGetCompositionStringW(
                    self.imc,
                    which,
                    wide_buf.as_mut_ptr() as *mut _,
                    byte_size as u32,
                )
            };
            OsString::from_wide(&wide_buf).into_string()
        } else {
            Ok(String::new())
        }
    }
}

impl Drop for ImmContext {
    fn drop(&mut self) {
        unsafe {
            ImmReleaseContext(self.hwnd, self.imc);
        }
    }
}

unsafe fn ime_set_context(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    let inner = rc_from_hwnd(hwnd)?;
    let inner = inner.borrow_mut();

    if inner.config.ime_preedit_rendering == ImePreeditRendering::System {
        return None;
    }

    // Don't show system CompositionWindow because application itself draws it
    let lparam = lparam & !(ISC_SHOWUICOMPOSITIONWINDOW as LPARAM);
    let result = DefWindowProcW(hwnd, msg, wparam, lparam);
    Some(result)
}

unsafe fn ime_end_composition(
    hwnd: HWND,
    _msg: UINT,
    _wparam: WPARAM,
    _lparam: LPARAM,
) -> Option<LRESULT> {
    // IME was cancelled
    let inner = rc_from_hwnd(hwnd)?;
    let mut inner = inner.borrow_mut();

    if inner.config.ime_preedit_rendering == ImePreeditRendering::System {
        return None;
    }

    inner
        .events
        .dispatch(WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None));
    Some(1)
}

unsafe fn ime_composition(
    hwnd: HWND,
    _msg: UINT,
    _wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    let inner = rc_from_hwnd(hwnd)?;
    let mut inner = inner.borrow_mut();

    if inner.config.ime_preedit_rendering == ImePreeditRendering::System {
        return None;
    }

    let imc = ImmContext::get(hwnd);

    let lparam = lparam as DWORD;

    if lparam == 0 {
        // IME was cancelled
        inner
            .events
            .dispatch(WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None));
        return Some(1);
    }

    if lparam & GCS_RESULTSTR == 0 {
        // No finished result; continue with the default
        // processing
        if let Ok(composing) = imc.get_str(GCS_COMPSTR) {
            inner
                .events
                .dispatch(WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::Composing(
                    composing,
                )));
        }
        // We will show the composing string ourselves.
        // Suppress the default composition display.
        return Some(1);
    }

    match imc.get_str(GCS_RESULTSTR) {
        Ok(s) if !s.is_empty() => {
            let key = KeyEvent {
                key: KeyCode::Composed(s),
                modifiers: Modifiers::NONE,
                leds: KeyboardLedStatus::empty(),
                repeat_count: 1,
                key_is_down: true,
                raw: None,
                win32_uni_char: None,
            };
            inner
                .events
                .dispatch(WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None));
            inner.events.dispatch(WindowEvent::KeyEvent(key));

            return Some(1);
        }
        Ok(_) => {}
        Err(_) => eprintln!("cannot represent IME as unicode string!?"),
    };
    None
}

/// Holds information about the current keyboard layout.
/// This is used to determine whether the layout includes
/// an AltGr key or just has a regular Right-Alt key,
/// as well as to build out information about dead keys.
struct KeyboardLayoutInfo {
    layout: HKL,
    has_alt_gr: bool,
    dead_keys: HashMap<(Modifiers, u8), DeadKey>,
}

#[derive(Debug)]
struct DeadKey {
    dead_char: char,
    _vk: u8,
    _mods: Modifiers,
    map: HashMap<(Modifiers, u8), char>,
}

#[derive(Debug)]
enum ResolvedDeadKey {
    InvalidDeadKey,
    Combined(char),
    InvalidCombination(char),
}

impl KeyboardLayoutInfo {
    pub fn new() -> Self {
        Self {
            layout: std::ptr::null_mut(),
            has_alt_gr: false,
            dead_keys: HashMap::new(),
        }
    }

    unsafe fn clear_key_state() {
        let mut out = [0u16; 16];
        let state = [0u8; 256];
        let scan = MapVirtualKeyW(VK_DECIMAL as _, MAPVK_VK_TO_VSC);
        // keep clocking the state to clear out its effects
        while ToUnicode(
            VK_DECIMAL as _,
            scan,
            state.as_ptr(),
            out.as_mut_ptr(),
            out.len() as i32,
            0,
        ) < 0
        {}
    }

    /// Probe to detect whether an AltGr key is present.
    /// This is done by synthesizing a keyboard state with control and alt
    /// pressed and then testing the virtual key presses.  If we find that
    /// one of these yields a single unicode character output then we assume that
    /// it does have AltGr.
    unsafe fn probe_alt_gr(&mut self) {
        self.has_alt_gr = false;

        let mut state = [0u8; 256];
        state[VK_CONTROL as usize] = 0x80;
        state[VK_MENU as usize] = 0x80;

        for vk in 0..=255u32 {
            if vk == VK_PACKET as u32 {
                // Avoid false positives
                continue;
            }

            let mut out = [0u16; 16];
            let ret = ToUnicode(vk, 0, state.as_ptr(), out.as_mut_ptr(), out.len() as i32, 0);
            if ret == 1 {
                self.has_alt_gr = true;
                break;
            }

            if ret == -1 {
                // Dead key.
                // keep clocking the state to clear out its effects
                while ToUnicode(vk, 0, state.as_ptr(), out.as_mut_ptr(), out.len() as i32, 0) < 0 {}
            }
        }
    }

    fn apply_mods(mods: Modifiers, state: &mut [u8; 256]) {
        if mods.contains(Modifiers::SHIFT) {
            state[VK_SHIFT as usize] = 0x80;
        }
        if mods.contains(Modifiers::CTRL) || mods.contains(Modifiers::RIGHT_ALT) {
            state[VK_CONTROL as usize] = 0x80;
        }
        if mods.contains(Modifiers::RIGHT_ALT) || mods.contains(Modifiers::ALT) {
            state[VK_MENU as usize] = 0x80;
        }
    }

    /// Probe the keymap to figure out which keys are dead keys
    unsafe fn probe_dead_keys(&mut self) {
        self.dead_keys.clear();

        let shift_states = [
            Modifiers::NONE,
            Modifiers::SHIFT,
            Modifiers::SHIFT | Modifiers::CTRL,
            Modifiers::ALT,
            Modifiers::RIGHT_ALT, // AltGr
        ];

        for &mods in &shift_states {
            let mut state = [0u8; 256];
            Self::apply_mods(mods, &mut state);

            for vk in 0..=255u32 {
                if vk == VK_PACKET as u32 {
                    // Avoid false positives
                    continue;
                }

                let scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);

                Self::clear_key_state();
                let mut out = [0u16; 16];
                let ret = ToUnicode(
                    vk,
                    scan,
                    state.as_ptr(),
                    out.as_mut_ptr(),
                    out.len() as i32,
                    0,
                );

                if ret != -1 {
                    continue;
                }

                // Found a Dead key.
                let dead_char = std::char::from_u32_unchecked(out[0] as u32);

                let mut map = HashMap::new();

                for &smod in &shift_states {
                    let mut second_state = [0u8; 256];
                    Self::apply_mods(smod, &mut second_state);

                    for ik in 0..=255u32 {
                        // Re-initiate the dead key starting state
                        Self::clear_key_state();
                        if ToUnicode(
                            vk,
                            scan,
                            state.as_ptr(),
                            out.as_mut_ptr(),
                            out.len() as i32,
                            0,
                        ) != -1
                        {
                            continue;
                        }

                        let scan = MapVirtualKeyW(ik, MAPVK_VK_TO_VSC);

                        let ret = ToUnicode(
                            ik,
                            scan,
                            second_state.as_ptr(),
                            out.as_mut_ptr(),
                            out.len() as i32,
                            0,
                        );

                        if ret == 1 {
                            // Found a combination
                            let c = std::char::from_u32_unchecked(out[0] as u32);
                            // clock through again to get the base
                            ToUnicode(
                                ik,
                                scan,
                                second_state.as_ptr(),
                                out.as_mut_ptr(),
                                out.len() as i32,
                                0,
                            );
                            let base = std::char::from_u32_unchecked(out[0] as u32);

                            if ((smod == Modifiers::CTRL)
                                || (smod == Modifiers::CTRL | Modifiers::SHIFT))
                                && c == base
                                && (c as u32) < 0x20
                            {
                                continue;
                            }

                            log::trace!(
                                "{:?}: {:?} {:?} + {:?} {:?} -> {:?} base={:?}",
                                dead_char,
                                mods,
                                vk,
                                smod,
                                ik,
                                c,
                                base
                            );

                            map.insert((smod, ik as u8), c);
                        }
                    }
                }

                self.dead_keys.insert(
                    (mods, vk as u8),
                    DeadKey {
                        dead_char,
                        _mods: mods,
                        _vk: vk as u8,
                        map,
                    },
                );
            }
        }
        Self::clear_key_state();
    }

    unsafe fn update(&mut self) {
        let current_layout = GetKeyboardLayout(0);
        if current_layout == self.layout {
            // Avoid recomputing this if the layout hasn't changed
            return;
        }

        let mut saved_state = [0u8; 256];
        if GetKeyboardState(saved_state.as_mut_ptr()) == 0 {
            return;
        }

        self.probe_alt_gr();
        self.probe_dead_keys();
        log::trace!("dead_keys: {:#x?}", self.dead_keys);

        SetKeyboardState(saved_state.as_mut_ptr());
        self.layout = current_layout;
    }

    pub fn has_alt_gr(&mut self) -> bool {
        unsafe {
            self.update();
        }
        self.has_alt_gr
    }

    /// Similar to Modifiers::remove_positional_mods except that it preserves
    /// RIGHT_ALT
    fn fixup_mods(mods: Modifiers) -> Modifiers {
        mods - (Modifiers::LEFT_SHIFT
            | Modifiers::RIGHT_SHIFT
            | Modifiers::LEFT_CTRL
            | Modifiers::RIGHT_CTRL
            | Modifiers::LEFT_ALT)
    }

    pub fn is_dead_key_leader(&mut self, mods: Modifiers, vk: u32) -> Option<char> {
        unsafe {
            self.update();
        }
        if vk <= (u8::MAX as u32) {
            self.dead_keys
                .get(&(Self::fixup_mods(mods), vk as u8))
                .map(|dead| dead.dead_char)
        } else {
            None
        }
    }

    pub fn resolve_dead_key(
        &mut self,
        leader: (Modifiers, u32),
        key: (Modifiers, u32),
    ) -> ResolvedDeadKey {
        unsafe {
            self.update();
        }
        if leader.1 <= (u8::MAX as u32) && key.1 <= (u8::MAX as u32) {
            if let Some(dead) = self
                .dead_keys
                .get(&(Self::fixup_mods(leader.0), leader.1 as u8))
            {
                if let Some(c) = dead
                    .map
                    .get(&(Self::fixup_mods(key.0), key.1 as u8))
                    .map(|&c| c)
                {
                    ResolvedDeadKey::Combined(c)
                } else {
                    ResolvedDeadKey::InvalidCombination(dead.dead_char)
                }
            } else {
                ResolvedDeadKey::InvalidDeadKey
            }
        } else {
            ResolvedDeadKey::InvalidDeadKey
        }
    }
}

/// Generate a MSG and call TranslateMessage upon it
unsafe fn translate_message(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM) {
    TranslateMessage(&MSG {
        hwnd,
        message: msg,
        wParam: wparam,
        lParam: lparam,
        pt: POINT { x: 0, y: 0 },
        time: GetTickCount(),
    });
}

unsafe fn key(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    let inner = rc_from_hwnd(hwnd)?;
    let mut inner = inner.borrow_mut();
    let repeat = (lparam & 0xffff) as u16;
    let scan_code = ((lparam >> 16) & 0xff) as u8;
    let releasing = (lparam & (1 << 31)) != 0;
    let ime_active = wparam == VK_PROCESSKEY as WPARAM;
    let phys_code = super::keycodes::vkey_to_phys(wparam);

    let alt_pressed = (lparam & (1 << 29)) != 0;
    let is_extended = (lparam & (1 << 24)) != 0;
    let was_down = (lparam & (1 << 30)) != 0;
    let label = match msg {
        WM_CHAR => "WM_CHAR",
        WM_IME_CHAR => "WM_IME_CHAR",
        WM_KEYDOWN => "WM_KEYDOWN",
        WM_KEYUP => "WM_KEYUP",
        WM_SYSKEYUP => "WM_SYSKEYUP",
        WM_SYSKEYDOWN => "WM_SYSKEYDOWN",
        WM_SYSCHAR => "WM_SYSCHAR",
        WM_DEADCHAR => "WM_DEADCHAR",
        _ => "WAT",
    };
    log::trace!(
        "{} c=`{}` repeat={} scan={} is_extended={} alt_pressed={} was_down={} \
             releasing={} IME={} dead_pending={:?}",
        label,
        wparam,
        repeat,
        scan_code,
        is_extended,
        alt_pressed,
        was_down,
        releasing,
        ime_active,
        inner.dead_pending,
    );

    if ime_active {
        // If the IME is active, allow Windows to perform default processing
        // to drive it forwards.  It will generate a call to `ime_composition`
        // or `ime_endcomposition` when it completes.

        if msg == WM_KEYDOWN {
            // Explicitly allow the built-in translation to occur for the IME
            translate_message(hwnd, msg, wparam, lparam);
            return Some(0);
        }

        return None;
    }

    if msg == WM_DEADCHAR {
        // Ignore WM_DEADCHAR; we only care about the resultant WM_CHAR
        return Some(0);
    }

    let keys = {
        let mut keys = [0u8; 256];
        GetKeyboardState(keys.as_mut_ptr());
        keys
    };

    let mut modifiers = Modifiers::default();
    if keys[VK_SHIFT as usize] & 0x80 != 0 {
        modifiers |= Modifiers::SHIFT;
    }
    if keys[VK_LSHIFT as usize] & 0x80 != 0 {
        modifiers |= Modifiers::LEFT_SHIFT;
    }
    if keys[VK_RSHIFT as usize] & 0x80 != 0 {
        modifiers |= Modifiers::RIGHT_SHIFT;
    }
    if keys[VK_LCONTROL as usize] & 0x80 != 0 {
        modifiers |= Modifiers::LEFT_CTRL;
    }
    if keys[VK_RCONTROL as usize] & 0x80 != 0 {
        modifiers |= Modifiers::RIGHT_CTRL;
    }
    modifiers.set(Modifiers::ENHANCED_KEY, is_extended);

    if inner.keyboard_info.has_alt_gr()
        && (keys[VK_RMENU as usize] & 0x80 != 0)
        && (keys[VK_CONTROL as usize] & 0x80 != 0)
    {
        // AltGr is pressed; while AltGr is on the RHS of the keyboard
        // is not the same thing as right-alt.
        // Windows sets RMENU and CONTROL to indicate AltGr and we
        // have to keep these in the key state in order for ToUnicode
        // to map the key correctly.
        // We set RIGHT_ALT as a hint to ourselves that AltGr is in
        // use (we use regular ALT otherwise) so that our dead key
        // resolution can do the right thing.
        modifiers |= Modifiers::RIGHT_ALT;
    } else if inner.keyboard_info.has_alt_gr()
        && inner.config.treat_left_ctrlalt_as_altgr
        && (keys[VK_MENU as usize] & 0x80 != 0)
        && (keys[VK_CONTROL as usize] & 0x80 != 0)
    {
        // When running inside a VNC session, VNC emulates the AltGr keypresses
        // by sending plain VK_MENU (rather than VK_RMENU) + VK_CONTROL.
        // For compatibility with that the option `treat_left_ctrlalt_as_altgr` allows
        // to treat MENU+CONTROL as equivalent to RMENU+CONTROL (AltGr) even though it is
        // technically a lossy transformation.
        //
        // We only do that when the keyboard layout has AltGr and the option is enabled,
        // so that we don't screw things up by default or for other keyboard layouts.
        // See issue #392 & #472 for some more context.
        modifiers |= Modifiers::RIGHT_ALT;
    } else {
        if keys[VK_CONTROL as usize] & 0x80 != 0 {
            modifiers |= Modifiers::CTRL;
        }
        if keys[VK_MENU as usize] & 0x80 != 0 {
            modifiers |= Modifiers::ALT;
        }
    }
    if keys[VK_LWIN as usize] & 0x80 != 0 || keys[VK_RWIN as usize] & 0x80 != 0 {
        modifiers |= Modifiers::SUPER;
    }

    let mut leds = KeyboardLedStatus::empty();
    if keys[VK_CAPITAL as usize] & 1 != 0 {
        leds |= KeyboardLedStatus::CAPS_LOCK;
    }
    if keys[VK_NUMLOCK as usize] & 1 != 0 {
        leds |= KeyboardLedStatus::NUM_LOCK;
    }

    let handled_raw = Handled::new();
    let raw_key_event = RawKeyEvent {
        key: match phys_code {
            Some(phys) => KeyCode::Physical(phys),
            None => KeyCode::RawCode(wparam as _),
        },
        phys_code,
        raw_code: wparam as _,
        scan_code: scan_code as _,
        leds,
        modifiers,
        repeat_count: 1,
        key_is_down: !releasing,
        handled: handled_raw.clone(),
    };

    let (key, win32_uni_char) = if msg == WM_IME_CHAR || msg == WM_CHAR {
        // If we were sent a character by the IME, some other apps,
        // or by ourselves via TranslateMessage, then take that
        // value as-is.
        (
            Some(KeyCode::Char(std::char::from_u32_unchecked(wparam as u32))),
            None,
        )
    } else {
        // Otherwise we're dealing with a raw key message.
        // ToUnicode has frustrating statefulness so we take care to
        // call it only when we think it will give consistent results.

        inner
            .events
            .dispatch(WindowEvent::RawKeyEvent(raw_key_event.clone()));
        if handled_raw.is_handled() {
            // Cancel any pending dead key
            if inner.dead_pending.take().is_some() {
                inner
                    .events
                    .dispatch(WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None));
            }
            log::trace!("raw key was handled; not processing further");
            return Some(0);
        }

        let is_modifier_only = phys_code.map(|p| p.is_modifier()).unwrap_or(false);
        if is_modifier_only {
            // If this event is only modifiers then don't ask the system
            // for further resolution, as we don't want ToUnicode to
            // perturb its inscrutable global state.
            // Modifier-only keypresses are reported as NUL when using win32 input mode.
            (phys_code.map(|p| p.to_key_code()), Some('\x00'))
        } else {
            // If we think this might be a dead key, process it for ourselves.
            // Our KeyboardLayoutInfo struct probed the layout for the key
            // combinations that start a dead key sequence, as well as those
            // that are valid end states for dead keys, so we can resolve
            // these for ourselves in a couple of quick hash lookups.
            let vk = wparam as u32;

            if releasing && inner.dead_pending.is_some() {
                // Don't care about key-up events while processing dead keys
                return Some(0);
            }

            // If we previously had the start of a dead key...
            let dead = if let Some(leader) = inner.dead_pending.take() {
                inner
                    .events
                    .dispatch(WindowEvent::AdviseDeadKeyStatus(DeadKeyStatus::None));
                // look to see how the current event resolves it
                match inner
                    .keyboard_info
                    .resolve_dead_key(leader, (modifiers, vk))
                {
                    // Valid combination produces a single character
                    ResolvedDeadKey::Combined(c) => Some(KeyCode::Char(c)),
                    ResolvedDeadKey::InvalidCombination(c) => {
                        // An invalid combination results in the deferred
                        // keypress triggering the original key first,
                        // and then we process the current key.

                        // Emit an event for the leader of the failed
                        // dead key combination
                        let key = KeyEvent {
                            key: KeyCode::Char(c),
                            modifiers,
                            leds,
                            repeat_count: 1,
                            key_is_down: !releasing,
                            win32_uni_char: Some(c),
                            raw: Some(RawKeyEvent {
                                scan_code: 0,
                                ..raw_key_event.clone()
                            }),
                        }
                        .normalize_shift()
                        .resurface_positional_modifier_key()
                        .normalize_ctrl();

                        inner.events.dispatch(WindowEvent::KeyEvent(key.clone()));

                        // And then we'll perform normal processing on the
                        // current key press
                        if let Some(new_dead_char) =
                            inner.keyboard_info.is_dead_key_leader(modifiers, vk)
                        {
                            if new_dead_char != c {
                                // Happens to be the start of its own new,
                                // different, dead key sequence
                                inner.dead_pending.replace((modifiers, vk));
                                return Some(0);
                            }

                            // They pressed the same dead key twice,
                            // emit the underlying char again and call
                            // it done.
                            // <https://github.com/wezterm/wezterm/issues/1729>
                            inner.events.dispatch(WindowEvent::KeyEvent(key.clone()));
                            return Some(0);
                        }

                        // We don't know; allow normal ToUnicode processing
                        None
                    }

                    // We thought we had a dead key last time around,
                    // but this time it didn't resolve.  Most likely
                    // because the keyboard layout changed in the middle
                    // of the keypress.
                    // We're effectively swallowing the original dead
                    // key event here, but we could potentially re-process
                    // the original and current one here if needed.
                    // Seems like a real edge case.
                    ResolvedDeadKey::InvalidDeadKey => None,
                }
            } else if let Some(c) = inner.keyboard_info.is_dead_key_leader(modifiers, vk) {
                if releasing {
                    // Don't care about key-up events while processing dead keys
                    return Some(0);
                }

                // They pressed a dead key.
                // If they want dead key processing, then record that and
                // wait for a subsequent keypress.
                if inner.config.use_dead_keys {
                    inner.dead_pending.replace((modifiers, vk));
                    inner.events.dispatch(WindowEvent::AdviseDeadKeyStatus(
                        DeadKeyStatus::Composing(c.to_string()),
                    ));
                    return Some(0);
                }
                // They don't want dead keys; just return the base character
                Some(KeyCode::Char(c))
            } else {
                // Not a dead key as far as we know
                None
            };

            if dead.is_some() {
                (dead, None)
            } else {
                // We get here for the various UP (but not DOWN as we shortcircuit
                // those above) messages.
                // We perform conversion to unicode for ourselves,
                // rather than calling TranslateMessage to do it for us,
                // so that we have tighter control over the key processing.
                let mut out = [0u16; 16];

                let win32_uni_char = {
                    let res = ToUnicode(
                        wparam as u32,
                        scan_code as u32,
                        keys.as_ptr(),
                        out.as_mut_ptr(),
                        out.len() as i32,
                        0,
                    );

                    match res {
                        1 => Some(std::char::from_u32_unchecked(out[0] as u32)),
                        0 => Some('\x00'),
                        _ => None,
                    }
                };

                let mut keys = keys;
                // If control is pressed, clear that out and remember it in our
                // own set of modifiers.
                // We used to also remove shift from this set, but it impacts
                // handling of eg: ctrl+shift+' (which is equivalent to ctrl+" in a US English
                // layout.
                // The shift normalization is now handled by the normalize_shift() method.
                if modifiers.contains(Modifiers::CTRL) {
                    keys[VK_CONTROL as usize] = 0;
                    keys[VK_LCONTROL as usize] = 0;
                    keys[VK_RCONTROL as usize] = 0;
                }

                let res = ToUnicode(
                    wparam as u32,
                    scan_code as u32,
                    keys.as_ptr(),
                    out.as_mut_ptr(),
                    out.len() as i32,
                    0,
                );

                let key = match res {
                    1 => Some(KeyCode::Char(std::char::from_u32_unchecked(out[0] as u32))),
                    // No mapping, so use our raw info
                    0 => {
                        log::trace!(
                            "ToUnicode had no mapping for {:?} wparam={}",
                            phys_code,
                            wparam
                        );
                        phys_code.map(|p| p.to_key_code())
                    }
                    _ => {
                        // dead key: if our dead key mapping in KeyboardLayoutInfo was
                        // correct, we shouldn't be able to get here as we should have
                        // landed in the dead key case above.
                        // If somehow we do get here, we don't have a valid mapping
                        // as -1 indicates the start of a dead key sequence,
                        // and any other n > 1 indicates an ambiguous expansion.
                        // Either way, indicate that we don't have a valid result.
                        log::error!(
                            "unexpected dead key expansion: \
                             modifiers={:?} vk={:?} res={} releasing={} {:?}",
                            modifiers,
                            vk,
                            res,
                            releasing,
                            out
                        );
                        KeyboardLayoutInfo::clear_key_state();
                        None
                    }
                };

                (key, win32_uni_char)
            }
        }
    };

    if let Some(key) = key {
        // FIXME: verify this behavior: Urgh, special case for ctrl and non-latin layouts.
        // In order to avoid a situation like #678, if CTRL is the only
        // modifier and we've got composed text, then discard the composed
        // text.
        let key = KeyEvent {
            key,
            modifiers,
            leds,
            repeat_count: repeat,
            key_is_down: !releasing,
            win32_uni_char,
            raw: Some(raw_key_event),
        }
        .normalize_shift();

        // Special case for ALT-space to show the system menu, and
        // ALT-F4 to close the window.
        if key.modifiers == Modifiers::ALT
            && (key.key == KeyCode::Char(' ') || key.key == KeyCode::Function(4))
        {
            translate_message(hwnd, msg, wparam, lparam);
            return None;
        }

        inner.events.dispatch(WindowEvent::KeyEvent(key));
        return Some(0);
    }
    None
}

unsafe fn drop_files(hwnd: HWND, _msg: UINT, wparam: WPARAM, _lparam: LPARAM) -> Option<LRESULT> {
    let inner = rc_from_hwnd(hwnd)?;
    let h_drop = wparam as HDROP;

    // Get the number of files dropped
    let file_count = DragQueryFileW(h_drop, 0xFFFFFFFF, null_mut(), 0);

    let mut filenames: Vec<PathBuf> = Vec::with_capacity(file_count as usize);

    for idx in 0..file_count {
        // The returned size of buffer is in characters, not including the terminating null character
        let buf_size = DragQueryFileW(h_drop, idx, null_mut(), 0);
        if buf_size > 0 {
            // Windows will truncate the filename and add null terminator if space isn't enough
            let buf_size = buf_size as usize + 1;
            let mut wide_buf = vec![0u16; buf_size];
            DragQueryFileW(h_drop, idx, wide_buf.as_mut_ptr(), wide_buf.len() as u32);
            wide_buf.pop(); // Drops the null terminator
            filenames.push(OsString::from_wide(&wide_buf).into());
        }
    }

    let mut inner = inner.borrow_mut();
    // WM_DROPFILES does carry a drop point (DragQueryPoint), but nothing here
    // is built or tested on Windows; reporting no position keeps the drop on
    // the long-standing "paste the paths into the terminal" path.
    inner.events.dispatch(WindowEvent::DroppedFile {
        paths: filenames,
        coords: None,
    });

    DragFinish(h_drop);
    Some(0)
}

unsafe fn do_wnd_proc(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    match msg {
        WM_NCCREATE => wm_nccreate(hwnd, msg, wparam, lparam),
        WM_NCDESTROY => wm_ncdestroy(hwnd, msg, wparam, lparam),
        WM_NCCALCSIZE => wm_nccalcsize(hwnd, msg, wparam, lparam),
        WM_NCHITTEST => wm_nchittest(hwnd, msg, wparam, lparam),
        WM_PAINT => wm_paint(hwnd, msg, wparam, lparam),
        WM_ENTERSIZEMOVE | WM_EXITSIZEMOVE => wm_enter_exit_size_move(hwnd, msg, wparam, lparam),
        WM_GETDPISCALEDSIZE => wm_get_dpi_scaled_size(hwnd, msg, wparam, lparam),
        WM_DPICHANGED => wm_dpi_changed(hwnd, msg, wparam, lparam),
        WM_WINDOWPOSCHANGED => wm_windowposchanged(hwnd, msg, wparam, lparam),
        WM_SETFOCUS => wm_set_focus(hwnd, msg, wparam, lparam),
        WM_KILLFOCUS => wm_kill_focus(hwnd, msg, wparam, lparam),
        WM_DEADCHAR | WM_KEYDOWN | WM_KEYUP | WM_SYSCHAR | WM_CHAR | WM_IME_CHAR | WM_SYSKEYUP
        | WM_SYSKEYDOWN => key(hwnd, msg, wparam, lparam),
        WM_SIZING => {
            // Allow events to be processed during live resize
            crate::spawn::SPAWN_QUEUE.run();
            None
        }
        WM_SETTINGCHANGE | WM_DWMCOMPOSITIONCHANGED => apply_theme(hwnd),
        WM_IME_SETCONTEXT => ime_set_context(hwnd, msg, wparam, lparam),
        WM_IME_COMPOSITION => ime_composition(hwnd, msg, wparam, lparam),
        WM_IME_ENDCOMPOSITION => ime_end_composition(hwnd, msg, wparam, lparam),
        WM_MOUSEMOVE => mouse_move(hwnd, msg, wparam, lparam),
        WM_MOUSELEAVE => mouse_leave(hwnd, msg, wparam, lparam),
        WM_MOUSEHWHEEL | WM_MOUSEWHEEL => mouse_wheel(hwnd, msg, wparam, lparam),
        WM_LBUTTONDBLCLK | WM_RBUTTONDBLCLK | WM_MBUTTONDBLCLK | WM_LBUTTONDOWN | WM_LBUTTONUP
        | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP => {
            mouse_button(hwnd, msg, wparam, lparam)
        }
        WM_DROPFILES => drop_files(hwnd, msg, wparam, lparam),
        WM_ERASEBKGND => Some(1),
        WM_CLOSE => {
            if let Some(inner) = rc_from_hwnd(hwnd) {
                let mut inner = inner.borrow_mut();
                inner.events.dispatch(WindowEvent::CloseRequested);
                // Don't let it close
                return Some(0);
            }
            None
        }
        _ => {
            if matches!(
                msg,
                WM_NCMOUSEMOVE | WM_NCMOUSELEAVE | WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK
            ) {
                let use_snap_layouts = !*IS_WIN10;
                if use_snap_layouts {
                    return match msg {
                        WM_NCMOUSEMOVE => nc_mouse_move(hwnd, msg, wparam, lparam),
                        WM_NCMOUSELEAVE => mouse_leave(hwnd, msg, wparam, lparam),
                        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK => {
                            nc_mouse_button(hwnd, msg, wparam, lparam)
                        }
                        _ => None,
                    };
                }
            }

            None
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match std::panic::catch_unwind(|| {
        do_wnd_proc(hwnd, msg, wparam, lparam)
            .unwrap_or_else(|| DefWindowProcW(hwnd, msg, wparam, lparam))
    }) {
        Ok(result) => result,
        Err(e) => {
            log::error!("caught {:?}", e);
            std::process::exit(1)
        }
    }
}

#[cfg(test)]
mod occlusion_tests {
    use super::window_is_effectively_visible;

    #[test]
    fn only_a_visible_non_iconic_window_is_effectively_visible() {
        assert!(window_is_effectively_visible(true, false));
        assert!(!window_is_effectively_visible(false, false));
        assert!(!window_is_effectively_visible(true, true));
        assert!(!window_is_effectively_visible(false, true));
    }
}
