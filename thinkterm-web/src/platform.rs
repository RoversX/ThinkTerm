//! What the App needs from the platform it runs on, and from the wire.
//!
//! The App is the same on a page, on a phone and (one day) on a desktop
//! shell: one type, generic over two seams. [`Platform`] is the page or the
//! shell -- clocks, timers, tasks, the frame callback, the viewport, the
//! keyboard field, the clipboard, the title. [`Link`] is the connection --
//! a WebSocket in the browser, ssh on a phone -- with the lease and the
//! handshakes the session layer expects of it.
//!
//! Everything here is single threaded: the App lives on one thread and the
//! futures it spawns are not `Send`. A platform that runs the App on its
//! own thread (the phone's core thread) drives its own executor and posts
//! the callbacks from there.

use crate::lease::Lease;
use codec::Pdu;
use std::cell::{Ref, RefMut};
use std::future::Future;
use std::pin::Pin;
use thinkterm_proto::TabId;
use thinkterm_session::host::{LinkError, PduLink};
use thinkterm_session::input::PaneLink;
use wezterm_term::KeyModifiers;

pub type LocalFuture<T> = Pin<Box<dyn Future<Output = T> + 'static>>;

/// The box the terminal is drawn in, as the platform sees it: its
/// position and size in the platform's own units (CSS px on a page,
/// points on a phone) and the device pixels per unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
    pub dpr: f64,
}

/// A press, move or release on the terminal, in the viewport's units
/// relative to the same origin as [`Viewport`].
#[derive(Debug, Clone, Copy)]
pub struct PointerInput {
    pub x: f64,
    pub y: f64,
    /// 0 left, 1 middle, 2 right; anything else is "none".
    pub button: u8,
    /// Whether any button is held (a move with none is a hover).
    pub buttons_down: bool,
    pub mods: KeyModifiers,
}

/// How far a wheel event asked to scroll, in the units it came in.
#[derive(Debug, Clone, Copy)]
pub enum WheelDelta {
    Lines(f64),
    Pages(f64),
    /// In the viewport's units (CSS px).
    Pixels(f64),
}

#[derive(Debug, Clone, Copy)]
pub struct WheelInput {
    pub x: f64,
    pub y: f64,
    /// Positive is down the page, towards the newest row.
    pub delta: WheelDelta,
    pub mods: KeyModifiers,
    /// Ctrl held: the platform's zoom, unless the event is synthetic.
    pub ctrl: bool,
    /// Whether a person produced it (a browser's `isTrusted`); a
    /// synthetic Ctrl+wheel is the page's pinch-to-zoom of the font.
    pub trusted: bool,
}

pub trait Platform: 'static {
    // ----- clocks -----

    /// Milliseconds from a clock that does not step (deadlines, backoff).
    fn monotonic_ms(&self) -> f64;
    /// Milliseconds since the Unix epoch (input serials, timestamps the
    /// wire defines as wall time).
    fn wall_ms(&self) -> f64;
    fn random_u32(&self) -> u32;
    /// The platform's length unit per inch, for turning points into
    /// pixels: 96 for CSS px, 72 for a phone's points.
    fn units_per_inch(&self) -> f64 {
        96.0
    }

    // ----- tasks and timers -----

    /// Run a future to completion on the App's thread.
    fn spawn(&self, fut: LocalFuture<()>);
    /// Call `cb` once, `delay_ms` from now, on the App's thread.
    fn set_timeout(&self, delay_ms: f64, cb: Box<dyn FnOnce()>);
    /// Call `cb` every `every_ms`, for the life of the platform.
    fn set_interval(&self, every_ms: f64, cb: Box<dyn FnMut()>);

    // ----- frames -----

    /// The App's paint, to be called from the platform's display callback
    /// after `request_frame`. Set once, at start.
    fn set_frame_handler(&self, cb: Box<dyn Fn()>);
    /// Ask for the frame handler to be called at the next display
    /// callback. Calls collapse: one frame per request, however many asks.
    fn request_frame(&self);

    // ----- the viewport -----

    fn viewport(&self) -> Viewport;
    /// The backing store is now `width` x `height` device pixels. Returns
    /// whether that cleared what was on screen (a canvas resize does), in
    /// which case the App paints at once rather than at the next frame.
    fn set_backing_size(&self, width: u32, height: u32) -> bool;
    /// The phone shape: no margin above and below the grid.
    fn is_mobile(&self) -> bool;

    // ----- the shell -----

    fn set_title(&self, title: &str);
    fn clipboard_write(&self, text: &str);
    /// Focus whatever the keyboard types into.
    fn focus_input(&self);
    /// The pointer's shape over the terminal (`text`, `col-resize`...).
    fn set_cursor(&self, cursor: &str);
    /// Where the cursor cell is, in the viewport's units, so the IME's
    /// candidate window can sit by it.
    fn set_ime_anchor(&self, anchor: crate::ime::Anchor);
    /// Something a probe or the display layer may read: the layout as
    /// JSON, the background colour. Keyed; the platform stores or ignores.
    fn publish(&self, key: &str, value: &str);
}

/// The connection, as the App drives it. The session layer's own traits
/// carry requests and the input lease; this adds the transport's life
/// (reconnect, shutdown), the pushes and the tab lease.
pub trait Link:
    PduLink<Request = LocalFuture<Result<Pdu, LinkError>>>
    + PaneLink<Prepare = LocalFuture<anyhow::Result<bool>>>
    + Clone
    + 'static
{
    /// Open the transport again to the same place with the same identity.
    fn reconnect(&self) -> LocalFuture<anyhow::Result<()>>;
    /// Close it for good: nothing will be read or sent again.
    fn shutdown(&self);
    fn lease(&self) -> Ref<'_, Lease>;
    fn lease_mut(&self) -> RefMut<'_, Lease>;
    /// Pushes (serial 0) go here; those that arrived before it was set
    /// are delivered at once.
    fn set_push_handler(&self, handler: Box<dyn FnMut(Pdu)>);
    /// The transport ended, and why.
    fn set_close_handler(&self, handler: Box<dyn FnMut(String)>);
    fn ensure_owner(&self, tab_id: TabId) -> LocalFuture<anyhow::Result<bool>>;
    fn claim(&self, tab_id: TabId) -> LocalFuture<anyhow::Result<bool>>;
    fn report_viewport(&self, tab_id: TabId) -> LocalFuture<anyhow::Result<()>>;
}
