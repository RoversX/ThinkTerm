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
mod host;
mod link;
pub mod painter;
mod ssh;
mod terminal;

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
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        &self,
        host: String,
        port: u16,
        user: String,
        key_path: String,
        remote_command: String,
        font_paths: Vec<String>,
        size_pt: f64,
        painter: Box<dyn painter::GlyphPainter>,
    ) {
        let _ = self.tx.send(core::Cmd::Connect {
            params: core::ConnectParams {
                ssh: ssh::SshParams {
                    host,
                    port,
                    user,
                    key_path,
                    remote_command,
                },
                font_paths,
                size_pt,
            },
            painter,
        });
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
