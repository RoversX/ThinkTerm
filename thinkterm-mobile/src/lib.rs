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
//! shell must treat as coming from a background thread. The views the
//! shell draws from go the other way without a wait: the core thread
//! publishes them after a change, and [`Core::views`] reads the latest.

mod core;
mod icons;
mod link;
pub mod painter;
mod platform;
mod qr;
mod ssh;
mod tailnet;

use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Authentication is blocked until the user approves this request's key:
    /// the jump host's (`connect`'s `jump`) when `jump`, else the host's.
    fn on_host_key_required(&self, request_id: String, fingerprint: String, jump: bool);
    /// Something the App publishes for the display layer, keyed: "bg" is
    /// the terminal's background as a hex colour, for the chrome around it;
    /// "link" is "down" from the moment the App's connection drops until it
    /// is back, "up" then, said on screen or off.
    fn on_published(&self, key: String, value: String);
    /// A pane rang its bell.
    fn on_bell(&self);
    /// The last rows of a pane, answering `request_preview`: JSON, a list
    /// of rows, each a list of `{"text","fg"}` runs (`App::PreviewRow`);
    /// or, answering `request_screen_preview`, an object: the whole tab the
    /// pane is in, each pane's screen where it lies (`App::TabPreview`).
    fn on_preview(&self, pane: u32, rows: String);
}

/// Rust's `log` output goes to stdout, which the simulator's console shows
/// (a device build will want os_log instead); on Android, to logcat under
/// the tag "thinkterm".
struct StdoutLogger;

impl log::Log for StdoutLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }
    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!("rust {} {}: {}", record.level(), record.target(), record.args());
        #[cfg(target_os = "android")]
        {
            if let Ok(c) = std::ffi::CString::new(line) {
                let prio = match record.level() {
                    log::Level::Error => ndk_sys::android_LogPriority::ANDROID_LOG_ERROR,
                    log::Level::Warn => ndk_sys::android_LogPriority::ANDROID_LOG_WARN,
                    _ => ndk_sys::android_LogPriority::ANDROID_LOG_INFO,
                };
                // SAFETY: both strings are NUL-terminated and outlive the call.
                unsafe {
                    ndk_sys::__android_log_write(prio.0 as std::ffi::c_int, c"thinkterm".as_ptr(), c.as_ptr());
                }
            }
            return;
        }
        #[allow(unreachable_code)]
        println!("{line}");
    }
    fn flush(&self) {}
}

/// The Android shell cannot reach an `ANativeWindow*` from Kotlin: these
/// two JNI entry points do it for `com.roversx.thinkterm.NativeWindow`.
/// `fromSurface` acquires a reference the shell releases with `release`
/// after `detach_surface`.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_com_roversx_thinkterm_NativeWindow_fromSurface(
    env: *mut jni_sys::JNIEnv,
    _class: jni_sys::jclass,
    surface: jni_sys::jobject,
) -> jni_sys::jlong {
    // SAFETY: the JVM hands over a live env and a Surface object.
    unsafe { ndk_sys::ANativeWindow_fromSurface(env as *mut _, surface as *mut _) as jni_sys::jlong }
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_com_roversx_thinkterm_NativeWindow_release(
    _env: *mut jni_sys::JNIEnv,
    _class: jni_sys::jclass,
    window: jni_sys::jlong,
) {
    if window != 0 {
        // SAFETY: a pointer `fromSurface` returned, released once.
        unsafe { ndk_sys::ANativeWindow_release(window as *mut _) }
    }
}

fn install_logger() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        static LOGGER: StdoutLogger = StdoutLogger;
        let _ = log::set_logger(&LOGGER);
        log::set_max_level(log::LevelFilter::Info);
        // A panic on the core thread ends it silently otherwise: stderr
        // goes nowhere on a phone.
        std::panic::set_hook(Box::new(|info| log::error!("panic: {info}")));
    });
}

/// The host a connection goes through on its way (`connect`'s `jump`),
/// signed in to with its own login: `auth_kind`, `secret` and `passphrase`
/// as `connect` takes the host's, `known_host` its own pinned key.
#[derive(uniffi::Record)]
pub struct JumpHost {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth_kind: String,
    pub secret: String,
    pub passphrase: Option<String>,
    pub known_host: Option<String>,
}

/// A login's secret as `connect` is given it.
fn auth_of(auth_kind: &str, secret: String, passphrase: Option<String>) -> ssh::Auth {
    match auth_kind {
        "password" => ssh::Auth::Password(secret),
        "key" => ssh::Auth::KeyPem {
            pem: secret,
            passphrase: passphrase.filter(|p| !p.is_empty()),
        },
        _ => ssh::Auth::KeyPath(secret),
    }
}

/// A key pair the shell asked for: the private half in the unencrypted
/// openssh-key-v1 container, the public half as one authorized_keys line.
#[derive(uniffi::Record)]
pub struct SshKeyPair {
    pub private_pem: String,
    pub public_line: String,
}

/// Every view the shell draws from, as JSON, made on the core thread
/// after a change ("null" while there is no App). `revision` grows with
/// each publication; the same number means nothing new.
#[derive(uniffi::Record, Clone)]
pub struct ViewSet {
    pub revision: u64,
    /// The connection they are from: each `connect` starts a later one, so
    /// a shell that just connected can tell views left from the one before.
    pub connection: u64,
    pub tabs: String,
    pub threads: String,
    pub tree: String,
    pub sidebar: String,
    pub navs: String,
    /// The panes of the tab on show (`App::panes_view`).
    pub panes: String,
    /// The Agents panel (`App::agents_view`).
    pub agents: String,
    pub status: String,
    pub layout: String,
}

impl Default for ViewSet {
    fn default() -> Self {
        let null = || "null".to_string();
        Self {
            revision: 0,
            connection: 0,
            tabs: null(),
            threads: null(),
            tree: null(),
            sidebar: null(),
            navs: null(),
            panes: null(),
            agents: null(),
            status: null(),
            layout: null(),
        }
    }
}

/// A card heading a tab or a pane's capsule, as the shell draws it: a
/// circle in `circle`, and on it the glyph's shape (`mask`, one byte a
/// pixel, `size` square: how much the shape covers) in `glyph`. The
/// colours are `#rrggbb`.
#[derive(uniffi::Record)]
pub struct TabIcon {
    pub circle: String,
    pub glyph: String,
    pub size: u32,
    pub mask: Vec<u8>,
}

/// Something drawn for the shell to show as it is: `width` by `height`
/// pixels, four bytes each, red first, the colours already multiplied by
/// how opaque they are.
#[derive(uniffi::Record)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// The icons of what a thread's panes run, as its row leads with them on
/// the desktop: the first card and at its corner the second, or a disc
/// for how many more there are.
#[derive(uniffi::Record)]
pub struct CardsPicture {
    pub picture: Picture,
    /// How far in from the picture's edges the icons' own box starts, in
    /// pixels: the room their shadows fall in.
    pub inset: u32,
    /// The box's width in pixels, the room a row keeps for its icons
    /// however many there are; it is as high as the first icon.
    pub slot: u32,
    /// The programs past the first, when a disc stands for them: the
    /// shell writes the number on it.
    pub count: Option<CardsCount>,
}

/// The disc in a `CardsPicture`: its corner in the picture and how wide
/// it is, in pixels.
#[derive(uniffi::Record)]
pub struct CardsCount {
    pub more: u32,
    pub x: f32,
    pub y: f32,
    pub diameter: f32,
}

/// A thread's mark as the desktop's sidebar draws it: one picture for a
/// mark at rest, and for one that moves a picture for each step of its
/// turn, the one on show being `(now / step_ms) % frames` by the wall
/// clock, so that every mark steps together. Each is `side` pixels square
/// with the mark in its middle, as a `Picture`'s pixels are.
#[derive(uniffi::Record)]
pub struct MarkPictures {
    pub side: u32,
    pub frames: Vec<Vec<u8>>,
    pub step_ms: u32,
}

/// The mark of a thread whose row says `status` and `dot` (a `ThreadRow`'s,
/// as the sidebar's rows spell them) and whether it is `live`, `size`
/// pixels across; `status` "Failed" for a row that says `failed`, which
/// has a mark of its own. `dark` is the appearance of what it is shown on,
/// whose text a mark at rest may be coloured as. None for words no row
/// says.
#[uniffi::export]
pub fn thread_mark(status: String, dot: String, live: bool, size: u32, dark: bool) -> Option<MarkPictures> {
    use thinkterm_tab_icons::look;
    let word = |word: String| serde_json::Value::String(word);
    let (mark, tone) = if status == "Failed" {
        thinkterm_web::tree::failed_mark()
    } else {
        let status = serde_json::from_value(word(status)).ok()?;
        let dot = serde_json::from_value(word(dot)).ok()?;
        thinkterm_web::tree::thread_mark(status, dot, live)
    };
    let (color, alpha) = match tone {
        look::Tone::Color(color) => (look::status_srgb(color, dark), 1.0),
        look::Tone::Text(alpha) if dark => (thinkterm_tab_icons::WHITE, alpha),
        look::Tone::Text(alpha) => (thinkterm_tab_icons::Rgb(0, 0, 0), alpha),
    };
    let size = (size as f32).clamp(1.0, icons::MAX_PICTURE / look::TICK_SCALE);
    let side = look::mark_side(size) as u32;
    let steps = if mark == look::Mark::Working { look::MARK_STEPS } else { 1 };
    let frames = (0..steps)
        .map(|step| icons::picture(&look::mark_svg(mark, Some(step), size, color, alpha), side, side))
        .collect::<Option<Vec<_>>>()?;
    Some(MarkPictures { side, frames, step_ms: look::MARK_STEP_MS as u32 })
}

/// How an upload stands (`Core::upload_status`): how much of it has gone,
/// and once it is `done` where the file is on the host and that path as a
/// shell reads it, a space after it -- what the desktop pastes for a file
/// dropped on a remote terminal -- or why there is none.
#[derive(uniffi::Record, Clone, Default)]
pub struct UploadStatus {
    pub sent: u64,
    pub total: u64,
    pub done: bool,
    pub path: String,
    pub paste: String,
    pub error: String,
}

/// A file sent with what is said in a chat: one uploaded first
/// (`upload_file`), by where it is on the host.
#[derive(uniffi::Record, Clone, Default)]
pub struct ChatAttachment {
    /// Its name, as the person picked it.
    pub name: String,
    /// A picture, which the agent is shown as one; else a file whose
    /// path it is told.
    pub image: bool,
    /// Where it is on the host (`UploadStatus::path`).
    pub path: String,
}

/// What the core thread last published for the shell to read. The lock
/// is held only to copy in or out, never while anything is made.
pub(crate) struct Published {
    pub views: ViewSet,
    /// The focused pane's visible rows (`App::screen_text`), kept current
    /// only while the shell wants them (`Core::set_screen_wanted`), with
    /// a count of their own: output changes them and no view.
    pub screen: String,
    pub screen_revision: u64,
    /// The uploads under way and those done whose end the shell has yet
    /// to read (`Core::upload_status`, which forgets them), by id.
    pub uploads: std::collections::HashMap<u64, UploadStatus>,
}

impl Default for Published {
    fn default() -> Self {
        Self {
            views: ViewSet::default(),
            screen: "null".into(),
            screen_revision: 0,
            uploads: Default::default(),
        }
    }
}

/// The system's randomness, as ssh-key's generator wants it.
struct SysRng(ring::rand::SystemRandom);

impl rand_core::TryRng for SysRng {
    type Error = std::convert::Infallible;
    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        let mut b = [0u8; 4];
        self.try_fill_bytes(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        let mut b = [0u8; 8];
        self.try_fill_bytes(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }
    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        use ring::rand::SecureRandom;
        self.0.fill(dst).expect("the system's randomness is available");
        Ok(())
    }
}

impl rand_core::TryCryptoRng for SysRng {}

/// A fresh ed25519 key pair, with `comment` on both halves.
#[uniffi::export]
pub fn generate_ssh_key(comment: String) -> Option<SshKeyPair> {
    use russh::keys::ssh_key::{Algorithm, LineEnding, PrivateKey};
    let mut key = PrivateKey::random(&mut SysRng(ring::rand::SystemRandom::new()), Algorithm::Ed25519).ok()?;
    key.set_comment(comment.as_str());
    let pem = key.to_openssh(LineEnding::LF).ok()?;
    let public_line = key.public_key().to_openssh().ok()?;
    Some(SshKeyPair { private_pem: pem.to_string(), public_line })
}

/// The public line of a private key the user pasted or imported, with
/// `comment` on it; none when the text is not a key (or the passphrase
/// is wrong).
#[uniffi::export]
pub fn ssh_public_line(pem: String, passphrase: Option<String>, comment: String) -> Option<String> {
    let key = russh::keys::decode_secret_key(&pem, passphrase.as_deref().filter(|p| !p.is_empty())).ok()?;
    let mut public = key.public_key().clone();
    public.set_comment(comment.as_str());
    public.to_openssh().ok()
}

#[derive(uniffi::Object)]
pub struct Core {
    tx: Sender<core::Cmd>,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// What the core thread last published.
    published: std::sync::Arc<Mutex<Published>>,
    /// The shell reads views from `published`; until it does, the core
    /// makes none there (a shell that pulls each one with `view` never does).
    views_read: std::sync::Arc<AtomicBool>,
    /// The cards the App puts on tabs and capsules: the built-in ones,
    /// then the connected host's.
    icons: std::sync::Arc<icons::Shown>,
}

#[uniffi::export]
impl Core {
    #[uniffi::constructor]
    pub fn new(notify: Box<dyn Notify>) -> std::sync::Arc<Self> {
        install_logger();
        let (tx, rx) = mpsc::channel();
        let published = std::sync::Arc::new(Mutex::new(Published::default()));
        let views_read = std::sync::Arc::new(AtomicBool::new(false));
        let icons = std::sync::Arc::new(icons::Shown::new(icons::catalog()));
        let thread = std::thread::Builder::new()
            .name("thinkterm-core".into())
            .spawn({
                let tx = tx.clone();
                let published = std::sync::Arc::clone(&published);
                let views_read = std::sync::Arc::clone(&views_read);
                let icons = std::sync::Arc::clone(&icons);
                move || core::run(tx, rx, notify, published, views_read, icons)
            })
            .expect("spawning the core thread");
        std::sync::Arc::new(Self {
            tx,
            thread: Mutex::new(Some(thread)),
            published,
            views_read,
            icons,
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
    /// earlier connection, if any. `jump` is the host this one is reached
    /// through, if any; a host of the built-in tailnet is reached over it
    /// instead, as on the desktop. `device_id` names this install to the
    /// server, the same every launch: a tab this phone held is still its
    /// own after a relaunch, rather than "another device's".
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        &self,
        request_id: String,
        host: String,
        port: u16,
        user: String,
        auth_kind: String,
        secret: String,
        passphrase: Option<String>,
        known_host: Option<String>,
        jump: Option<JumpHost>,
        remote_command: String,
        device_id: String,
        keepalive_secs: u32,
        font_paths: Vec<String>,
        size_pt: f64,
        painter: Box<dyn painter::GlyphPainter>,
    ) {
        let auth = auth_of(&auth_kind, secret, passphrase);
        let jump = jump.map(|jump| ssh::JumpParams {
            auth: auth_of(&jump.auth_kind, jump.secret, jump.passphrase),
            host: jump.host,
            port: jump.port,
            user: jump.user,
            known_host: jump.known_host.filter(|k| !k.is_empty()),
        });
        let _ = self.tx.send(core::Cmd::Connect {
            params: core::ConnectParams {
                ssh: ssh::SshParams {
                    request_id,
                    host,
                    port,
                    user,
                    auth,
                    known_host: known_host.filter(|k| !k.is_empty()),
                    remote_command,
                    keepalive_secs: keepalive_secs as u64,
                    jump,
                },
                font_paths,
                size_pt,
                device_id,
            },
            painter,
        });
    }

    // ----- the App's views and commands, for the shell's own interface -----

    /// The views the core thread last published, read without waiting
    /// for it; `on_change` says when there are new ones.
    pub fn views(&self) -> ViewSet {
        self.mark_views_read();
        self.published.lock().map(|p| p.views.clone()).unwrap_or_default()
    }

    pub fn views_revision(&self) -> u64 {
        self.mark_views_read();
        self.published.lock().map(|p| p.views.revision).unwrap_or(0)
    }

    /// The connection the published views are from (`ViewSet::connection`),
    /// without copying them.
    pub fn views_connection(&self) -> u64 {
        self.mark_views_read();
        self.published.lock().map(|p| p.views.connection).unwrap_or(0)
    }

    /// The focused pane's visible rows as the core last published them
    /// (`screen_text`'s JSON), current while the shell wants them. Output
    /// changes them without an `on_change`: `screen_revision` says when.
    pub fn screen(&self) -> String {
        self.published.lock().map(|p| p.screen.clone()).unwrap_or_else(|_| "null".into())
    }

    pub fn screen_revision(&self) -> u64 {
        self.published.lock().map(|p| p.screen_revision).unwrap_or(0)
    }

    /// Keep `screen` current: the shell's text input reads the rows (the
    /// keyboard is up, or there is a selection). Unwanted, they are not
    /// made at every paint; `screen_text` makes them on demand.
    pub fn set_screen_wanted(&self, wanted: bool) {
        let _ = self.tx.send(core::Cmd::WantScreen(wanted));
    }

    /// The card `id` (a tab's or a capsule's `icon`), its glyph drawn
    /// `size` pixels square; none for an id there is no card for. Drawn on
    /// the caller's thread, waiting on nothing: a shell keeps what it got
    /// while `tab_icons_revision` stays.
    pub fn tab_icon(&self, id: String, size: u32) -> Option<TabIcon> {
        let card = self.icons.card(&id)?;
        let size = size.clamp(1, icons::MAX_SIZE);
        // The shell tints a shape: a mark in its own colours is drawn as
        // its one-colour form, at the usual size.
        let svg = if card.full_color {
            thinkterm_tab_icons::builtin_shape(&card.id).unwrap_or(card.svg.as_str())
        } else {
            card.svg.as_str()
        };
        Some(TabIcon {
            mask: icons::mask(svg, size).unwrap_or_default(),
            circle: card.circle,
            glyph: card.glyph,
            size,
        })
    }

    /// The cards `ids` (a thread row's `icons`, or one tab's `icon`) as one
    /// picture, the first `diameter` pixels across, lit and shadowed as the
    /// desktop's are (`thinkterm_tab_icons::look`); `scale` is how many
    /// pixels a point is and `dark` the appearance of what they are shown
    /// on. With `glyphs` false the circles are bare, for a spinner. Ids
    /// there is no card for are left out; none for none left. Drawn on the
    /// caller's thread, and kept by the shell as `tab_icon`'s are.
    pub fn cards_picture(&self, ids: Vec<String>, diameter: u32, scale: f32, dark: bool, glyphs: bool) -> Option<CardsPicture> {
        use thinkterm_tab_icons::look;
        let cards: Vec<_> = ids.iter().filter_map(|id| self.icons.card(id)).collect();
        let diameter = (diameter as f32).clamp(1.0, icons::MAX_SIZE as f32);
        let drawn = look::cards_svg(&cards.iter().collect::<Vec<_>>(), diameter, scale.clamp(1.0, 4.0), dark, glyphs)?;
        let (width, height) = (drawn.width as u32, drawn.height as u32);
        Some(CardsPicture {
            picture: Picture { width, height, rgba: icons::picture(&drawn.svg, width, height)? },
            inset: drawn.inset as u32,
            slot: drawn.slot as u32,
            count: drawn.count.map(|count| CardsCount {
                more: count.more as u32,
                x: count.x,
                y: count.y,
                diameter: count.diameter,
            }),
        })
    }

    /// Counts the changes to the cards `tab_icon` draws: each connection
    /// brings its host's. What a shell drew before a change is stale.
    pub fn tab_icons_revision(&self) -> u64 {
        self.icons.revision()
    }

    /// The text between two visible cells of the focused pane, both ends
    /// inclusive, for the clipboard; the pane's own selection is left be.
    pub fn text_between(&self, anchor_row: u32, anchor_col: u32, head_row: u32, head_col: u32) -> Option<String> {
        let (reply, ack) = mpsc::channel();
        let cmd = core::Cmd::TextBetween {
            anchor: (anchor_row as usize, anchor_col as usize),
            head: (head_row as usize, head_col as usize),
            reply,
        };
        if self.tx.send(cmd).is_err() {
            return None;
        }
        ack.recv().unwrap_or(None)
    }

    /// Whether this connection's terminal is on screen with the app in
    /// front. Off screen it keeps its session but takes no tab from
    /// another device and marks no result as seen.
    pub fn set_shown(&self, shown: bool) {
        let _ = self.tx.send(core::Cmd::SetShown(shown));
    }

    /// An upload's id, for `upload_file`, `upload_status` and
    /// `cancel_upload`: asked as a file is chosen, before it is made ready
    /// to go (copied, loaded), which can take a while. Its path goes, once
    /// it is through, into the pane focused now -- not whichever has the
    /// focus by the time the file is ready. One whose file could not be
    /// made ready is given up (`cancel_upload`).
    pub fn upload_begin(&self) -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(core::Cmd::UploadFor(id));
        id
    }

    /// Leave this device's file at `path` on the host as `name`, in the
    /// folder the desktop puts what is dropped on a remote terminal
    /// (`~/ThinkTerm_Uploads`) and under another name where that one is
    /// taken, over the connection's own ssh session beside the terminal:
    /// upload `id` (`upload_begin`). The file is read as it goes and may
    /// be any size; it must stay where it is until the upload is done.
    /// Returns at once.
    pub fn upload_file(&self, id: u64, name: String, path: String) {
        let total = std::fs::metadata(&path).map(|file| file.len()).unwrap_or(0);
        if let Ok(mut published) = self.published.lock() {
            published.uploads.insert(id, UploadStatus { total, ..Default::default() });
        }
        if self.tx.send(core::Cmd::Upload { id, name, path }).is_err() {
            if let Ok(mut published) = self.published.lock() {
                published.uploads.remove(&id);
            }
        }
    }

    /// How upload `id` stands; none for one there is no word of. One
    /// that is done is forgotten once this has said so.
    pub fn upload_status(&self, id: u64) -> Option<UploadStatus> {
        let mut published = self.published.lock().ok()?;
        let mut status = published.uploads.get(&id)?.clone();
        if status.done {
            published.uploads.remove(&id);
            if status.error.is_empty() {
                match shlex::try_quote(&status.path) {
                    Ok(quoted) => status.paste = format!("{quoted} "),
                    Err(_) => status.error = "the host named the file oddly".into(),
                }
            }
        }
        Some(status)
    }

    /// Give upload `id` up: nothing of it is kept on the host. Also for
    /// one begun whose file could not be made ready.
    pub fn cancel_upload(&self, id: u64) {
        let _ = self.tx.send(core::Cmd::CancelUpload(id));
    }

    /// The device came back to the app, or its network changed: a
    /// connection that went with the wait is found out now (within a few
    /// seconds) rather than when its keep-alives give up, and one already
    /// down is tried again at once rather than after the wait its failures
    /// earned.
    pub fn check_connection(&self) {
        let _ = self.tx.send(core::Cmd::CheckConnection);
    }

    /// The host's key, pinned as `connect`'s `known_host`, was forgotten:
    /// the connection up now stays, and the dials it makes again by itself
    /// ask for the key the host has now (`on_host_key_required`) rather
    /// than hold it to the old one.
    pub fn forget_host_key(&self) {
        let _ = self.tx.send(core::Cmd::ForgetHostKey);
    }

    /// As `forget_host_key`, for the key of the jump host that `connect`
    /// was given.
    pub fn forget_jump_host_key(&self) {
        let _ = self.tx.send(core::Cmd::ForgetJumpHostKey);
    }

    /// Paste a finished upload's `paste` (`UploadStatus`) into the pane the
    /// upload was started from -- not whichever has the focus now -- if
    /// that pane is still on show; otherwise nothing is pasted.
    pub fn paste_upload(&self, id: u64, text: String) {
        let _ = self.tx.send(core::Cmd::PasteUpload { id, text });
    }

    /// Whether the shell shows one pane of a tab at a time, whole, for
    /// want of room for a split's panes side by side: the pane on show is
    /// zoomed and the others are listed in `ViewSet::panes`, a "pane"
    /// click on one showing it instead. Off, the split is drawn whole,
    /// each pane under its bar.
    pub fn set_one_pane(&self, one: bool) {
        let _ = self.tx.send(core::Cmd::OnePane(one));
    }

    /// Whether the shell shows this connection's views -- its threads, in
    /// a list with other hosts' -- while its terminal is off screen: they
    /// are kept current for it, which `set_shown(false)` alone stops.
    pub fn set_views_listed(&self, listed: bool) {
        let _ = self.tx.send(core::Cmd::ListViews(listed));
    }

    /// One of the App's views as JSON: "tabs", "threads", "tree",
    /// "sidebar", "navs", "status", "layout" or "strings", made while the
    /// caller waits; "null" while there is no App. `views` does not wait.
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

    /// Open or fold the sub-task list of the agent in `pane`, in the
    /// Agents panel (`agents`: a row's `subtasks_open`).
    pub fn agent_toggle_subtasks(&self, pane: u32) {
        let _ = self.tx.send(core::Cmd::AgentToggleSubtasks(pane));
    }

    /// Show the agent in `pane` as a chat: `agent` is its row's
    /// `agent_id`. The chat comes, and comes again whenever it changes, as
    /// JSON through `Notify::on_published` under "chat"
    /// (`thinkterm_agent_chat::view::ChatView`). One chat at a time: this
    /// closes the one before.
    pub fn chat_open(&self, pane: u32, agent: String) {
        let _ = self.tx.send(core::Cmd::Chat(core::ChatCmd::Open { pane, agent }));
    }

    pub fn chat_close(&self) {
        let _ = self.tx.send(core::Cmd::Chat(core::ChatCmd::Close));
    }

    /// Say `text` to the agent, with the files uploaded for it: a turn of
    /// its own, or added to the running one.
    pub fn chat_send(&self, text: String, attachments: Vec<ChatAttachment>) {
        let attachments = attachments
            .into_iter()
            .map(|file| thinkterm_agent_chat::Attachment {
                name: file.name,
                image: file.image,
                path: Some(file.path),
                thumb: None,
            })
            .collect();
        let _ = self.tx.send(core::Cmd::Chat(core::ChatCmd::Send { text, attachments }));
    }

    /// Talk to another model (an id of the view's `model.models`), or
    /// think harder or less (an id of its `model.efforts`).
    pub fn chat_set_model(&self, model: String, effort: Option<String>) {
        let _ = self.tx.send(core::Cmd::Chat(core::ChatCmd::SetModel { model, effort }));
    }

    /// Upload `id` is for a chat: its path is not pasted into a pane.
    pub fn upload_unpasted(&self, id: u64) {
        let _ = self.tx.send(core::Cmd::UploadUnpasted(id));
    }

    /// Answer the question on show (its `ask.id`) with one of its
    /// `choices` (a choice's `id`).
    pub fn chat_answer(&self, ask: String, choice: String) {
        let _ = self.tx.send(core::Cmd::Chat(core::ChatCmd::Answer { ask, choice }));
    }

    pub fn chat_interrupt(&self) {
        let _ = self.tx.send(core::Cmd::Chat(core::ChatCmd::Interrupt));
    }

    /// History from before the first row, when the chat says there is more.
    pub fn chat_older(&self) {
        let _ = self.tx.send(core::Cmd::Chat(core::ChatCmd::Older));
    }

    /// The chat's problem's action: "install" or "retry".
    pub fn chat_action(&self, action: String) {
        let cmd = match action.as_str() {
            "install" => core::ChatCmd::Install,
            _ => core::ChatCmd::Retry,
        };
        let _ = self.tx.send(core::Cmd::Chat(cmd));
    }

    /// Open or fold the chat's row `row` (a fold's `id`).
    pub fn chat_toggle(&self, row: String) {
        let _ = self.tx.send(core::Cmd::Chat(core::ChatCmd::Toggle(row)));
    }

    /// A touch on the terminal, in points: "down", "move" or "up".
    pub fn pointer(&self, kind: String, x: f64, y: f64) {
        let _ = self.tx.send(core::Cmd::Pointer { kind, x, y });
    }

    /// A mouse on the terminal, in points, as the desktop takes one: "down",
    /// "move" or "up" of `button` (0 left, 1 middle, 2 right), `held`
    /// whether a button is down -- a move with none is a hover -- and the
    /// modifiers held. A press focuses the pane under it; a drag selects,
    /// a double or triple press a word or a line; a program that asked for
    /// the mouse gets it, unless Shift holds it back for the selection.
    #[allow(clippy::too_many_arguments)]
    pub fn mouse(&self, kind: String, x: f64, y: f64, button: u8, held: bool, shift: bool, ctrl: bool, alt: bool) {
        let mut mods = wezterm_term::KeyModifiers::NONE;
        if shift {
            mods |= wezterm_term::KeyModifiers::SHIFT;
        }
        if ctrl {
            mods |= wezterm_term::KeyModifiers::CTRL;
        }
        if alt {
            mods |= wezterm_term::KeyModifiers::ALT;
        }
        let _ = self.tx.send(core::Cmd::Mouse { kind, x, y, button, held, mods });
    }

    /// Whether the program in the pane under a point, in points, asked for
    /// the mouse: a right click there goes to it, as on the desktop, and
    /// the copy and paste chip waits for one with Shift held.
    pub fn mouse_grabbed_at(&self, x: f64, y: f64) -> bool {
        let (reply, ack) = mpsc::channel();
        if self.tx.send(core::Cmd::MouseGrabbedAt { x, y, reply }).is_err() {
            return false;
        }
        ack.recv().unwrap_or(false)
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

    /// While following the host's scheme (`set_palette(None)`): the scheme
    /// to draw in instead when the host's configuration chose none -- the
    /// light default on a light screen, as the desktop does -- and
    /// `unchosen`, the scheme a host fills in when its configuration names
    /// none ("Apple System Colors"), which is no choice either. Both as
    /// `set_palette` takes them; none to draw the host's scheme whatever it
    /// is. Kept like a setting.
    pub fn set_fallback_palette(&self, scheme: Option<String>, unchosen: Option<String>) {
        let _ = self.tx.send(core::Cmd::SetFallbackPalette { scheme, unchosen });
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
    /// Ask for the last `rows` rows of any pane; `on_preview` answers.
    pub fn request_preview(&self, pane: u32, rows: u32) {
        let _ = self.tx.send(core::Cmd::Preview { pane: pane as usize, rows: rows as usize });
    }

    /// Ask for the whole tab any pane is in, as it is split, every pane's
    /// screen with every row at its own size; `on_preview` answers.
    pub fn request_screen_preview(&self, pane: u32) {
        let _ = self.tx.send(core::Cmd::ScreenPreview { pane: pane as usize });
    }

    pub fn selected_text(&self) -> Option<String> {
        let (reply, ack) = mpsc::channel();
        if self.tx.send(core::Cmd::SelectedText { reply }).is_err() {
            return None;
        }
        ack.recv().unwrap_or(None)
    }

    /// The pane focused now, the one a paste and the selection go to, with
    /// whatever was sent before this taken in; none without a connection.
    pub fn focused_pane(&self) -> Option<u32> {
        let (reply, ack) = mpsc::channel();
        if self.tx.send(core::Cmd::FocusedPane { reply }).is_err() {
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

impl Core {
    /// The first read of the views has the core publish them from then on,
    /// starting with the ones it has now.
    fn mark_views_read(&self) {
        if !self.views_read.swap(true, Ordering::Relaxed) {
            let _ = self.tx.send(core::Cmd::ViewsRead);
        }
    }
}
