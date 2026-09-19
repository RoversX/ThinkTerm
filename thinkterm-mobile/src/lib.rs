//! The mobile client's FFI surface.
//!
//! Everything a shell can call lives on [`Core`], a `Send + Sync` handle
//! that forwards to one core thread. UniFFI requires exported objects to be
//! callable from any thread; the session and (later) the App are single
//! threaded (`Rc`, non-`Send` futures), so the handle is a channel, not the
//! state. Commands are asynchronous except where the plan makes them a
//! handshake: `detach_surface` returns only once the core thread has
//! stopped touching the surface, because an Android `surfaceDestroyed` (and
//! a UIKit view going away) must not return before that.
//!
//! The core thread never waits on the shell's main thread. It talks back
//! only through [`Notify`] and [`painter::GlyphPainter`], whose calls the
//! shell must treat as coming from a background thread.

mod core;
mod link;
pub mod painter;
mod platform;
mod ssh;

use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use std::thread::JoinHandle;

uniffi::setup_scaffolding!();

/// What the core tells the shell. Called on the core thread.
#[uniffi::export(callback_interface)]
pub trait Notify: Send + Sync {
    /// The core has something to draw; the shell should call `render` from
    /// its display callback (CADisplayLink, Choreographer). Never draw from
    /// inside this call.
    fn on_frame_needed(&self);
    /// The connection's state in a line: connecting, attached to a pane,
    /// disconnected and why.
    fn on_status(&self, status: String);
    /// A line for the shell's log view.
    fn on_log(&self, line: String);
    /// The focused pane's title.
    fn on_title(&self, title: String);
    /// Something the App shows changed (tabs, the tree, the status); the
    /// shell reads the views it wants.
    fn on_change(&self);
    /// Text the App wants on the clipboard.
    fn on_clipboard(&self, text: String);
    /// The App wants the keyboard.
    fn on_focus_input(&self);
    /// Where the cursor cell is, in points, in the terminal view.
    fn on_ime_anchor(&self, left: f64, top: f64, width: f64, height: f64);
    /// The host's key fingerprint, seen before authentication: the shell
    /// remembers it and passes it back as `known_host` next time.
    fn on_host_key(&self, fingerprint: String);
    /// Something the App publishes for the display layer, keyed: "bg" is
    /// the terminal's background as a hex colour, for the chrome around it.
    fn on_published(&self, key: String, value: String);
}

/// Rust's `log` output goes to stdout, which the simulator's console shows;
/// a device build will want os_log instead.
struct StdoutLogger;

impl log::Log for StdoutLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            println!(
                "rust {} {}: {}",
                record.level(),
                record.target(),
                record.args()
            );
        }
    }
    fn flush(&self) {}
}

fn install_logger() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        static LOGGER: StdoutLogger = StdoutLogger;
        let _ = log::set_logger(&LOGGER);
        log::set_max_level(log::LevelFilter::Info);
    });
}

#[derive(uniffi::Object)]
pub struct Core {
    tx: Sender<core::Cmd>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

#[uniffi::export]
impl Core {
    #[uniffi::constructor]
    pub fn new(notify: Box<dyn Notify>) -> std::sync::Arc<Self> {
        install_logger();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("thinkterm-core".into())
            .spawn({
                let tx = tx.clone();
                move || core::run(tx, rx, notify)
            })
            .expect("spawning the core thread");
        std::sync::Arc::new(Self {
            tx,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Give the core a CAMetalLayer (`layer` is the unretained pointer) to
    /// draw on. Returns the surface generation the shell must quote back
    /// in `resize` and `detach_surface`, or 0 if the GPU could not be set
    /// up (the reason goes to `on_log`).
    pub fn attach_surface(&self, layer: u64, width: u32, height: u32, scale: f64) -> u64 {
        let (reply, ack) = mpsc::channel();
        if self
            .tx
            .send(core::Cmd::Attach {
                layer: layer as usize,
                width,
                height,
                scale,
                reply,
            })
            .is_err()
        {
            return 0;
        }
        ack.recv().unwrap_or(0)
    }

    /// The surface's drawable size changed (rotation, keyboard, split
    /// view). Asynchronous; a stale generation is ignored.
    pub fn resize(&self, generation: u64, width: u32, height: u32, scale: f64) {
        let _ = self.tx.send(core::Cmd::Resize {
            generation,
            width,
            height,
            scale,
        });
    }

    /// The handshake: returns once the core thread has dropped the surface
    /// and will not touch the layer again. The only call a shell may block
    /// its main thread on.
    pub fn detach_surface(&self, generation: u64) {
        let (reply, ack) = mpsc::channel();
        if self
            .tx
            .send(core::Cmd::Detach { generation, reply })
            .is_ok()
        {
            let _ = ack.recv();
        }
    }

    /// Draw one frame if a surface is attached. Asynchronous: the shell
    /// calls this from its display callback and returns immediately.
    pub fn render(&self) {
        let _ = self.tx.send(core::Cmd::Render);
    }

    /// Start or stop the demo animation drawn while no pane is attached.
    pub fn set_animating(&self, on: bool) {
        let _ = self.tx.send(core::Cmd::Animate(on));
    }

    /// Connect over ssh, exec the proxy on the host and attach to its
    /// active pane. `font_paths` are the TTF files to shape with (base face
    /// first); `painter` draws what they lack.
    /// `auth_kind` is "key-path" (the secret is a file path), "key" (the
    /// secret is the private key's text, `passphrase` its passphrase) or
    /// "password". `known_host` is the fingerprint remembered from an
    /// earlier connection, if any. `device_id` names this install to the
    /// server, the same every launch: a tab this phone held is still its
    /// own after a relaunch, rather than "another device's".
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        &self,
        host: String,
        port: u16,
        user: String,
        auth_kind: String,
        secret: String,
        passphrase: Option<String>,
        known_host: Option<String>,
        remote_command: String,
        device_id: String,
        font_paths: Vec<String>,
        size_pt: f64,
        painter: Box<dyn painter::GlyphPainter>,
    ) {
        let auth = match auth_kind.as_str() {
            "password" => ssh::Auth::Password(secret),
            "key" => ssh::Auth::KeyPem {
                pem: secret,
                passphrase: passphrase.filter(|p| !p.is_empty()),
            },
            _ => ssh::Auth::KeyPath(secret),
        };
        let _ = self.tx.send(core::Cmd::Connect {
            params: core::ConnectParams {
                ssh: ssh::SshParams {
                    host,
                    port,
                    user,
                    auth,
                    known_host: known_host.filter(|k| !k.is_empty()),
                    remote_command,
                },
                font_paths,
                size_pt,
                device_id,
            },
            painter,
        });
    }

    // ----- the App's views and commands, for the shell's own interface -----

    /// One of the App's views as JSON: "tabs", "sidebar", "navs",
    /// "status", "layout" or "strings". "null" while there is no App.
    pub fn view(&self, name: String) -> String {
        let (reply, ack) = mpsc::channel();
        if self.tx.send(core::Cmd::View { name, reply }).is_err() {
            return "null".into();
        }
        ack.recv().unwrap_or_else(|_| "null".into())
    }

    /// A click on the tab strip or a pane's bar: "pane", "new-tab",
    /// "new-pane", "split-right", "split-below", "zoom", "close",
    /// "close-pane", "close-tab", "follow".
    pub fn chrome_click(&self, action: String, pane: Option<u32>, tab: Option<u32>) {
        let _ = self.tx.send(core::Cmd::ChromeClick { action, pane, tab });
    }

    /// A click in the sidebar: "thread", "window", "toggle-project",
    /// "new-thread", "pin", "delete", "archive", "unarchive"...
    pub fn side_click(&self, kind: String, id: Option<String>, flag: Option<bool>) {
        let _ = self.tx.send(core::Cmd::SideClick { kind, id, flag });
    }

    /// A key in the sidebar's text field ("Enter" commits, "Escape"
    /// cancels) with the field's text.
    pub fn side_key(&self, key: String, value: String) {
        let _ = self.tx.send(core::Cmd::SideKey { key, value });
    }

    /// The context menu for `kind` ("pane", "tab", "thread", "project",
    /// "space"...) and `id`, as JSON.
    pub fn context_menu(&self, kind: String, id: String) -> String {
        let (reply, ack) = mpsc::channel();
        if self
            .tx
            .send(core::Cmd::ContextMenu { kind, id, reply })
            .is_err()
        {
            return "[]".into();
        }
        ack.recv().unwrap_or_else(|_| "[]".into())
    }

    /// Do what a menu row asks; the outcome says whether it was handled
    /// and whether the shell should copy or paste.
    pub fn menu_action(&self, id: String) -> String {
        let (reply, ack) = mpsc::channel();
        if self.tx.send(core::Cmd::MenuAction { id, reply }).is_err() {
            return "{\"handled\":false,\"copy\":null,\"paste\":false}".into();
        }
        ack.recv().unwrap_or_default()
    }

    pub fn set_space(&self, id: String) {
        let _ = self.tx.send(core::Cmd::SetSpace(id));
    }

    pub fn take_over(&self) {
        let _ = self.tx.send(core::Cmd::TakeOver);
    }

    /// A touch on the terminal, in points: "down", "move" or "up".
    pub fn pointer(&self, kind: String, x: f64, y: f64) {
        let _ = self.tx.send(core::Cmd::Pointer { kind, x, y });
    }

    /// Scroll at a point by `lines`; positive is back into history.
    pub fn wheel(&self, x: f64, y: f64, lines: f64) {
        let _ = self.tx.send(core::Cmd::Wheel { x, y, lines });
    }

    /// Scroll at a point by `px` points of finger travel; positive is down
    /// the page, towards the newest row. The App keeps the fraction of a
    /// row, so this is what a drag with inertia sends.
    pub fn wheel_px(&self, x: f64, y: f64, px: f64) {
        let _ = self.tx.send(core::Cmd::WheelPx { x, y, px });
    }

    /// One of the App's preferences, as JSON: `set_setting("scroll-mode",
    /// "\"stepped\"")`. Kept by the core and applied to every App it makes.
    pub fn set_setting(&self, key: String, value: String) {
        let _ = self.tx.send(core::Cmd::SetSetting { key, value });
    }

    /// The terminal's colour scheme: one entry of `schemes.json` as JSON
    /// (`{"foreground":"#...","background":"#...","ansi":[...],...}`), or
    /// none to follow the host's own scheme. Kept like a setting.
    pub fn set_palette(&self, scheme: Option<String>) {
        let _ = self.tx.send(core::Cmd::SetPalette(scheme));
    }

    /// Scale the focused pane's font by a tenth per step; 0 resets.
    pub fn step_font(&self, by: f64) {
        let _ = self.tx.send(core::Cmd::StepFont(by));
    }

    /// The focused pane's visible rows as a document for the platform's
    /// text system, as JSON (`App::screen_text`); "null" without a pane.
    pub fn screen_text(&self) -> String {
        let (reply, ack) = mpsc::channel();
        if self.tx.send(core::Cmd::ScreenText { reply }).is_err() {
            return "null".into();
        }
        ack.recv().unwrap_or_else(|_| "null".into())
    }

    /// Select between two visible cells (row, col) of the focused pane.
    pub fn set_selection(&self, anchor_row: u32, anchor_col: u32, head_row: u32, head_col: u32) {
        let _ = self.tx.send(core::Cmd::SetSelection {
            anchor: (anchor_row as usize, anchor_col as usize),
            head: (head_row as usize, head_col as usize),
        });
    }

    pub fn clear_selection(&self) {
        let _ = self.tx.send(core::Cmd::ClearSelection);
    }

    /// The selected text of the focused pane, for the clipboard.
    pub fn selected_text(&self) -> Option<String> {
        let (reply, ack) = mpsc::channel();
        if self.tx.send(core::Cmd::SelectedText { reply }).is_err() {
            return None;
        }
        ack.recv().unwrap_or(None)
    }

    pub fn disconnect(&self) {
        let _ = self.tx.send(core::Cmd::Disconnect);
    }

    /// A key by its DOM name ("Enter", "Backspace", "ArrowUp", "c"...).
    pub fn key(&self, name: String, ctrl: bool, alt: bool, shift: bool) {
        let _ = self.tx.send(core::Cmd::Key {
            name,
            ctrl,
            alt,
            shift,
        });
    }

    /// The IME is composing (a Pinyin buffer is open, say): keys are the
    /// IME's until it ends, and nothing reaches the pane meanwhile.
    pub fn set_composing(&self, on: bool) {
        let _ = self.tx.send(core::Cmd::Composing(on));
    }

    /// Committed text from the soft keyboard or an IME.
    pub fn text(&self, text: String) {
        let _ = self.tx.send(core::Cmd::Text(text));
    }

    pub fn paste(&self, text: String) {
        let _ = self.tx.send(core::Cmd::Paste(text));
    }

    /// Scroll the view by whole lines; positive is back into history.
    pub fn scroll(&self, lines: i32) {
        let _ = self.tx.send(core::Cmd::Scroll(lines));
    }

    /// Counters for the probe's screen: frames drawn, attach/detach
    /// generations, last frame time, errors, connection state.
    pub fn stats(&self) -> String {
        let (reply, ack) = mpsc::channel();
        if self.tx.send(core::Cmd::Stats { reply }).is_err() {
            return "core thread gone".into();
        }
        ack.recv().unwrap_or_else(|_| "core thread gone".into())
    }

    /// Stop the core thread. Blocks until it has exited.
    pub fn shutdown(&self) {
        let _ = self.tx.send(core::Cmd::Shutdown);
        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }
    }
}
