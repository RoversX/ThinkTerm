//! Remembering where the main window was, and putting it back there.
//!
//! macOS does this for us: the window is given a frame autosave name and AppKit
//! keeps the frame in the user defaults. Windows has no such facility, so this
//! module is the equivalent -- it listens for
//! [`window::WindowEvent::WindowFrameChanged`], writes the placement into the
//! native settings, and hands it back to `TermWindow::new` at the next launch.
//!
//! Three things are worth knowing about it:
//!
//! * **One window owns the placement.** ThinkTerm can have several terminal
//!   windows; if they all restored the same rect they would open in a stack,
//!   and if they all saved it the last one to move would win. So exactly one
//!   window holds a claim at a time -- the first one created -- and only that
//!   window restores or records. When it closes the claim is released, and
//!   whichever window is still open takes it over on its next move.
//!
//! * **Writes are debounced.** A drag produces a message per mouse-move; the
//!   settings file is rewritten 500ms after the last of them, on a worker
//!   thread, and synchronously on close so that a normal quit cannot race the
//!   process exit. A crash loses at most the last half second of dragging.
//!
//! * **A remembered rect is checked against the displays attached now**, not
//!   the ones it was saved on. See [`placement_is_usable`].

use crate::native_settings::{self, NativeMainWindowPlacement, NativeScreenRect};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};
use window::ScreenRect;

/// How long the window has to stay still before its placement is written.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Anything smaller than this is not a window someone arranged, it is damage:
/// a truncated settings file, a hand edit, a window that was mid-teardown when
/// the rect was read.
const MIN_FRAME_WIDTH: i32 = 200;
const MIN_FRAME_HEIGHT: i32 = 120;

/// How much of the window's width has to be on a display for it to be worth
/// reopening there -- enough to see and to grab.
const MIN_VISIBLE_WIDTH: i32 = 120;

/// The band at the top of the window that has to land inside a work area. It
/// does not have to be the real title bar height; it has to be enough of one
/// that the window can be dragged back out of wherever it ended up.
const TITLE_BAR_HEIGHT: i32 = 32;

/// The window that owns the saved placement, as `mux_window_id + 1`, or 0 for
/// "no window holds the claim". Offset by one so that window 0 is not confused
/// with the vacant state.
static MAIN_WINDOW: AtomicUsize = AtomicUsize::new(0);

/// The placement to open a window at.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RestoredFrame {
    pub(crate) frame: ScreenRect,
    pub(crate) maximized: bool,
}

/// Claim the main-window role for this window, or confirm it already holds it.
///
/// Called once when a window is created -- deciding whether it restores the
/// saved placement -- and again on every frame change, which is what lets a
/// surviving window pick up the role after the window that held it closed.
pub(crate) fn claim_main_window(mux_window_id: usize) -> bool {
    let token = mux_window_id.wrapping_add(1);
    match MAIN_WINDOW.compare_exchange(0, token, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => true,
        Err(current) => current == token,
    }
}

fn holds_claim(mux_window_id: usize) -> bool {
    MAIN_WINDOW.load(Ordering::Acquire) == mux_window_id.wrapping_add(1)
}

/// The window is going away. Write out whatever the debounce is still holding
/// -- synchronously, because the process may be seconds from exiting -- and let
/// another window take the role over.
pub(crate) fn window_closed(mux_window_id: usize) {
    if !holds_claim(mux_window_id) {
        return;
    }
    flush();
    MAIN_WINDOW.store(0, Ordering::Release);
}

/// The window may be going away: a close was requested, but something may yet
/// cancel it. Get the placement on disk without giving up the claim.
pub(crate) fn close_requested(mux_window_id: usize) {
    if holds_claim(mux_window_id) {
        flush();
    }
}

/// Record where the main window now is. Cheap to call from the event handler:
/// the file write happens later, on another thread, and only if the window
/// stays put.
pub(crate) fn record(mux_window_id: usize, frame: ScreenRect, maximized: bool) {
    if !claim_main_window(mux_window_id) {
        return;
    }
    if !native_settings::restore_main_window_frame_enabled() {
        return;
    }

    let frame = NativeScreenRect {
        x: frame.origin.x as i32,
        y: frame.origin.y as i32,
        width: frame.size.width as i32,
        height: frame.size.height as i32,
    };

    let placement = NativeMainWindowPlacement {
        x: frame.x,
        y: frame.y,
        width: frame.width,
        height: frame.height,
        maximized,
        work_area: work_area_for(frame),
    };

    let (lock, cvar) = debounce_state();
    {
        let mut pending = lock.lock().unwrap_or_else(|err| err.into_inner());
        pending.placement = Some(placement);
        pending.deadline = Instant::now() + DEBOUNCE;
    }
    cvar.notify_all();
}

/// Write any pending placement now, on this thread.
pub(crate) fn flush() {
    let Some(state) = DEBOUNCE_STATE.get() else {
        return;
    };
    let placement = {
        let mut pending = state.0.lock().unwrap_or_else(|err| err.into_inner());
        pending.placement.take()
    };
    if let Some(placement) = placement {
        native_settings::save_main_window_placement(placement);
    }
}

/// The frame the main window should open at, if one was saved and it still
/// makes sense on this desktop.
pub(crate) fn frame_to_restore() -> Option<RestoredFrame> {
    let placement = native_settings::main_window_placement()?;
    let frame = NativeScreenRect {
        x: placement.x,
        y: placement.y,
        width: placement.width,
        height: placement.height,
    };

    let work_areas = work_areas();
    if work_areas.is_empty() {
        // Nothing to check the rect against. Opening blind risks a window
        // nobody can reach, so take the default geometry instead.
        log::warn!("no display work areas reported; ignoring the saved main window placement");
        return None;
    }

    if !placement_is_usable(frame, &work_areas) {
        log::warn!(
            "saved main window placement {frame:?} does not land on any \
             attached display (work areas {work_areas:?}); using the default geometry"
        );
        return None;
    }

    Some(RestoredFrame {
        frame: euclid::rect(
            frame.x as isize,
            frame.y as isize,
            frame.width as isize,
            frame.height as isize,
        ),
        maximized: placement.maximized,
    })
}

/// Is a remembered frame still somewhere a person could use it?
///
/// Displays get unplugged, resolutions change, a laptop comes back from the
/// dock with one screen instead of three. The rule is deliberately about the
/// *title bar* rather than about area: a window is recoverable exactly when
/// enough of its top edge is on a work area to see it and drag it. So the
/// frame must overlap some attached display's work area by at least
/// [`MIN_VISIBLE_WIDTH`] horizontally (or its whole width, if it is narrower
/// than that), and its top edge must sit within that work area with at least
/// [`TITLE_BAR_HEIGHT`] to spare below.
///
/// Note that this is checked against the work areas reported *now*. The work
/// area stored beside the placement records what it was measured against; it
/// is not what grants permission to use it.
fn placement_is_usable(frame: NativeScreenRect, work_areas: &[NativeScreenRect]) -> bool {
    if frame.width < MIN_FRAME_WIDTH || frame.height < MIN_FRAME_HEIGHT {
        return false;
    }

    work_areas.iter().any(|area| {
        let overlap = (frame.x + frame.width).min(area.x + area.width) - frame.x.max(area.x);
        overlap >= MIN_VISIBLE_WIDTH.min(frame.width)
            && frame.y >= area.y
            && frame.y <= area.y + area.height - TITLE_BAR_HEIGHT
    })
}

struct Pending {
    placement: Option<NativeMainWindowPlacement>,
    deadline: Instant,
}

static DEBOUNCE_STATE: OnceLock<(Mutex<Pending>, Condvar)> = OnceLock::new();

fn debounce_state() -> &'static (Mutex<Pending>, Condvar) {
    let state = DEBOUNCE_STATE.get_or_init(|| {
        (
            Mutex::new(Pending {
                placement: None,
                deadline: Instant::now(),
            }),
            Condvar::new(),
        )
    });

    static WORKER: OnceLock<()> = OnceLock::new();
    WORKER.get_or_init(|| {
        std::thread::Builder::new()
            .name("main-window-placement".to_string())
            .spawn(|| debounce_worker(DEBOUNCE_STATE.get().expect("initialized above")))
            .map(|_| ())
            .unwrap_or_else(|err| {
                log::warn!("unable to start the window placement writer: {err:#}");
            })
    });

    state
}

fn debounce_worker(state: &'static (Mutex<Pending>, Condvar)) {
    let (lock, cvar) = state;
    loop {
        let placement = {
            let mut pending = lock.lock().unwrap_or_else(|err| err.into_inner());
            loop {
                if pending.placement.is_none() {
                    // Nothing to write: either nothing has moved yet, or
                    // `flush` took the pending value out from under us. Wait
                    // for the next edge rather than spinning.
                    pending = cvar.wait(pending).unwrap_or_else(|err| err.into_inner());
                    continue;
                }
                // Something is pending; it goes out once the window has been
                // still for `DEBOUNCE`. Every further move pushes the deadline
                // out, so a long drag costs exactly one write.
                let now = Instant::now();
                if now >= pending.deadline {
                    break;
                }
                let wait = pending.deadline - now;
                pending = cvar
                    .wait_timeout(pending, wait)
                    .unwrap_or_else(|err| err.into_inner())
                    .0;
            }
            pending.placement.take()
        };

        if let Some(placement) = placement {
            native_settings::save_main_window_placement(placement);
        }
    }
}

/// The work areas of every display attached right now.
#[cfg(windows)]
fn work_areas() -> Vec<NativeScreenRect> {
    use winapi::shared::minwindef::{LPARAM, TRUE};
    use winapi::shared::windef::{HDC, HMONITOR, LPRECT};
    use winapi::um::winuser::{EnumDisplayMonitors, GetMonitorInfoW, MONITORINFO};

    unsafe extern "system" fn callback(
        mon: HMONITOR,
        _hdc: HDC,
        _rect: LPRECT,
        data: LPARAM,
    ) -> i32 {
        let areas = &mut *(data as *mut Vec<NativeScreenRect>);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(mon, &mut mi) != 0 {
            areas.push(NativeScreenRect {
                x: mi.rcWork.left,
                y: mi.rcWork.top,
                width: mi.rcWork.right - mi.rcWork.left,
                height: mi.rcWork.bottom - mi.rcWork.top,
            });
        }
        TRUE
    }

    let mut areas: Vec<NativeScreenRect> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(callback),
            &mut areas as *mut _ as LPARAM,
        );
    }
    areas
}

/// The work area of the display the window is on, recorded next to the
/// placement so a later launch can tell whether the desktop still looks the
/// way it did.
#[cfg(windows)]
fn work_area_for(frame: NativeScreenRect) -> NativeScreenRect {
    use winapi::shared::windef::RECT;
    use winapi::um::winuser::{
        GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };

    let rect = RECT {
        left: frame.x,
        top: frame.y,
        right: frame.x + frame.width,
        bottom: frame.y + frame.height,
    };

    unsafe {
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let mon = MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST);
        if GetMonitorInfoW(mon, &mut mi) != 0 {
            NativeScreenRect {
                x: mi.rcWork.left,
                y: mi.rcWork.top,
                width: mi.rcWork.right - mi.rcWork.left,
                height: mi.rcWork.bottom - mi.rcWork.top,
            }
        } else {
            NativeScreenRect::default()
        }
    }
}

/// Only the Windows backend reports frame changes or honours a restored frame,
/// so everywhere else this is inert: nothing is ever recorded, and a settings
/// file carrying a placement from a Windows machine is ignored rather than
/// applied to a desktop whose displays we cannot enumerate here.
#[cfg(not(windows))]
fn work_areas() -> Vec<NativeScreenRect> {
    Vec::new()
}

#[cfg(not(windows))]
fn work_area_for(_frame: NativeScreenRect) -> NativeScreenRect {
    NativeScreenRect::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> NativeScreenRect {
        NativeScreenRect {
            x,
            y,
            width,
            height,
        }
    }

    /// 1024x768 with a 40px taskbar along the bottom, which is what the
    /// machine this was developed on has.
    fn one_display() -> Vec<NativeScreenRect> {
        vec![rect(0, 0, 1024, 728)]
    }

    #[test]
    fn an_ordinary_window_comes_back() {
        assert!(placement_is_usable(rect(120, 90, 700, 500), &one_display()));
    }

    #[test]
    fn a_window_from_a_display_that_is_gone_does_not() {
        assert!(!placement_is_usable(
            rect(5000, 5000, 700, 500),
            &one_display()
        ));
    }

    /// The interesting rejection: the window is still horizontally on screen,
    /// but it was dragged down until the title bar is past the work area, so
    /// reopening it there would leave nothing to grab.
    #[test]
    fn a_title_bar_below_the_work_area_does_not() {
        assert!(!placement_is_usable(
            rect(120, 720, 700, 500),
            &one_display()
        ));
        // ... while one that still clears the title bar band does.
        assert!(placement_is_usable(
            rect(120, 690, 700, 500),
            &one_display()
        ));
    }

    #[test]
    fn a_window_hanging_off_the_right_edge_survives_if_enough_is_visible() {
        assert!(placement_is_usable(
            rect(900, 100, 700, 500),
            &one_display()
        ));
        assert!(!placement_is_usable(
            rect(960, 100, 700, 500),
            &one_display()
        ));
    }

    /// A window above the top of the work area has its title bar off screen on
    /// Windows, where nothing can be dragged down from there.
    #[test]
    fn a_window_above_the_work_area_does_not() {
        assert!(!placement_is_usable(
            rect(120, -40, 700, 500),
            &one_display()
        ));
    }

    #[test]
    fn a_degenerate_rect_does_not() {
        assert!(!placement_is_usable(rect(0, 0, 0, 0), &one_display()));
        assert!(!placement_is_usable(rect(10, 10, 199, 500), &one_display()));
        assert!(!placement_is_usable(rect(10, 10, 700, 119), &one_display()));
    }

    /// A second display to the left lives at negative coordinates, which the
    /// overlap arithmetic has to survive.
    #[test]
    fn a_window_on_a_secondary_display_comes_back() {
        let displays = vec![rect(0, 0, 1024, 728), rect(-1280, -200, 1280, 984)];
        assert!(placement_is_usable(rect(-1200, -150, 700, 500), &displays));
        // Unplug it and the same rect is no longer anywhere.
        assert!(!placement_is_usable(
            rect(-1200, -150, 700, 500),
            &one_display()
        ));
    }
}
