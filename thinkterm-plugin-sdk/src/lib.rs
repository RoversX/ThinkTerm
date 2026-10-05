//! Writing a ThinkTerm plugin.
//!
//! A plugin is a program that ThinkTerm starts when the plugin is first
//! used, and talks to over the program's standard input and output (see
//! [`protocol`], and docs/thinkterm/plugins.md for the manifest that goes
//! with it). Implement [`Plugin`] and hand it to [`run`]:
//!
//! ```no_run
//! use serde_json::{json, Value};
//! use thinkterm_plugin_sdk::{Cx, Plugin};
//!
//! struct Hello;
//!
//! impl Plugin for Hello {
//!     fn call(&mut self, body: Value, _cx: &mut Cx) -> anyhow::Result<Value> {
//!         match body["op"].as_str() {
//!             Some("greet") => Ok(json!("hello")),
//!             _ => anyhow::bail!("no such call"),
//!         }
//!     }
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     thinkterm_plugin_sdk::run(Hello)
//! }
//! ```
//!
//! A plugin with a panel in the right sidebar draws it by hand, from the
//! items in [`panel`], each time [`Plugin::draw`] is called:
//!
//! ```no_run
//! use thinkterm_plugin_sdk::panel::{Frame, Rect, Text, Token};
//! use thinkterm_plugin_sdk::{Plugin, View};
//!
//! struct Greeting;
//!
//! impl Plugin for Greeting {
//!     fn draw(&mut self, view: &View, frame: &mut Frame) {
//!         let line = view.env.body.line;
//!         frame.push(Rect::new(8.0, 8.0, view.env.width - 16.0, line + 12.0).fill(Token::BgRaised).radius(6.0));
//!         frame.push(Text::new(16.0, 14.0, view.env.width - 32.0, line, "Hello"));
//!     }
//! }
//! ```
//!
//! Text is typed in a [`Field`](panel::Field), which ThinkTerm draws and
//! edits itself: the plugin is sent what it holds as it changes, and when
//! the user presses Return, and keeps it with a
//! [`FieldText`](panel::FieldText). A panel the user gives the keyboard to
//! is sent the keys its frame says it takes ([`Frame::keys`](panel::Frame::keys())).
//!
//! A panel with more to show than a sidebar has room for asks for its
//! extended view, a wide area beside the sidebar that the user sizes, with
//! [`Frame::extend`](panel::Frame::extend()). It comes as a view of its own,
//! drawn the same way: [`View::extended`] tells the two apart, and
//! [`View::panel`] names the panel either belongs to, for what they share.
//!
//! When the terminal beside a panel runs on another machine, the view's
//! [`Env::remote`](panel::Env::remote) says which, and the plugin has
//! ThinkTerm run programs and read files there with [`Emitter::ask`], over
//! ThinkTerm's own connection to it.
//!
//! The plugins built into ThinkTerm implement the same trait, and run
//! inside the plugin host instead of in a program of their own.

pub mod protocol;

pub use thinkterm_plugin_panel as panel;

use anyhow::anyhow;
use panel::{Answer, Ask, Env, Focus, Frame, Input, Item, Rows, RowsWanted};
use protocol::{FromPlugin, ToPlugin};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thinkterm_plugin_channel::wire::Raw;

pub trait Plugin: Send {
    /// Answers a client's call: any JSON, as the plugin defines it. The
    /// default takes no calls.
    fn call(&mut self, body: Value, cx: &mut Cx) -> anyhow::Result<Value> {
        let _ = (body, cx);
        anyhow::bail!("this plugin takes no calls")
    }

    /// Draws the panel `view` shows into `frame`, which starts empty: when
    /// it comes on show, when its size, fonts or theme change, after each
    /// input, and whenever [`Cx::redraw`] or [`Emitter::redraw`] asks. A
    /// frame the same as the last one sent is not sent again. The default
    /// draws nothing.
    fn draw(&mut self, view: &View, frame: &mut Frame) {
        let _ = (view, frame);
    }

    /// Rows of a list [`draw`](Self::draw) put in `view`'s frame, as
    /// `wanted` says: at most `wanted.to - wanted.from` of them, from
    /// `wanted.from` on.
    fn rows(&mut self, view: &View, wanted: &RowsWanted) -> Vec<Vec<Item>> {
        let _ = (view, wanted);
        Vec::new()
    }

    /// What the user did in `view`: a click; in an extended view, the close
    /// button pressed -- the view goes, and the panel is to stop asking for
    /// it; text typed in a field, or submitted; the keyboard coming or
    /// going (`view.focus` follows it); a key the panel takes. The panel is
    /// drawn again after it, and its extended view with it.
    fn input(&mut self, view: &View, input: Input, cx: &mut Cx) {
        let _ = (view, input, cx);
    }

    /// `view` went off show.
    fn closed(&mut self, view: &View) {
        let _ = view;
    }
}

/// A plugin chosen at run time, among several.
impl<P: Plugin + ?Sized> Plugin for Box<P> {
    fn call(&mut self, body: Value, cx: &mut Cx) -> anyhow::Result<Value> {
        (**self).call(body, cx)
    }

    fn draw(&mut self, view: &View, frame: &mut Frame) {
        (**self).draw(view, frame)
    }

    fn rows(&mut self, view: &View, wanted: &RowsWanted) -> Vec<Vec<Item>> {
        (**self).rows(view, wanted)
    }

    fn input(&mut self, view: &View, input: Input, cx: &mut Cx) {
        (**self).input(view, input, cx)
    }

    fn closed(&mut self, view: &View) {
        (**self).closed(view)
    }
}

/// A panel of the plugin on show. The same plugin can be on show in
/// several places at once -- two windows, a browser -- each a view of its
/// own, with its own size. A panel's extended view is a view as well.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub id: u64,
    pub env: Env,
    /// For an extended view, the panel it extends.
    pub extends: Option<u64>,
    /// For a panel, its extended view while one is on show: the panel is
    /// drawn again when it comes and when it goes.
    pub extension: Option<u64>,
    /// Where the keyboard is in the view while it has it -- in which of
    /// its fields, if any -- as the last `Input::Focus` said; `None` while
    /// it is elsewhere.
    pub focus: Option<Focus>,
}

impl View {
    /// A panel of `env`'s size, numbered `id`.
    pub fn new(id: u64, env: Env) -> Self {
        Self {
            id,
            env,
            extends: None,
            extension: None,
            focus: None,
        }
    }

    /// Whether this is a panel's extended view.
    pub fn extended(&self) -> bool {
        self.extends.is_some()
    }

    /// The panel this view belongs to: itself, or the one it extends.
    pub fn panel(&self) -> u64 {
        self.extends.unwrap_or(self.id)
    }
}

/// What a call does besides answering, all of it sent once the answer is.
#[derive(Debug, Default)]
pub struct Cx {
    watch: bool,
    redraw: bool,
    events: Vec<Value>,
}

impl Cx {
    pub fn new() -> Self {
        Self::default()
    }

    /// The caller is sent this plugin's events from now on: a client that
    /// listed something, to hear when it changes.
    pub fn watch(&mut self) {
        self.watch = true;
    }

    /// Sends `event` to every client watching this plugin, the caller
    /// included.
    pub fn emit(&mut self, event: Value) {
        self.events.push(event);
    }

    /// Every panel of the plugin on show is drawn again: what they show
    /// changed.
    pub fn redraw(&mut self) {
        self.redraw = true;
    }

    /// What was asked for, for whatever runs the plugin to carry out.
    pub fn finish(self) -> Effects {
        Effects {
            watch: self.watch,
            redraw: self.redraw,
            events: self.events,
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Effects {
    pub watch: bool,
    pub redraw: bool,
    pub events: Vec<Value>,
}

/// Reaches the plugin from outside a call: from a thread the plugin keeps
/// to watch something, say.
#[derive(Debug, Clone)]
pub struct Emitter {
    wake: Sender<Wake>,
    /// A redraw is asked for and not yet done: more asks fold into it.
    redrawing: Arc<AtomicBool>,
    asking: Arc<Asking>,
}

impl Emitter {
    pub fn emit(&self, event: Value) -> io::Result<()> {
        send(&FromPlugin::Event { body: event })
    }

    /// Has every panel of the plugin on show drawn again, on the plugin's
    /// own thread, once it is done with what it is doing.
    pub fn redraw(&self) {
        if !self.redrawing.swap(true, Ordering::AcqRel) {
            let _ = self.wake.send(Wake::Redraw);
        }
    }

    /// Has ThinkTerm do `ask` on `machine`, the other machine the terminal
    /// beside panel `view` runs on as its env names it
    /// ([`Remote::machine`](panel::Remote::machine)), and waits at most
    /// `wait` for what came of it. It fails once the terminal has moved to
    /// another machine. Blocking: ask from a thread of the plugin's own
    /// rather than while drawing, which waits for it.
    pub fn ask(&self, view: u64, machine: &str, ask: &Ask, wait: Duration) -> Answer {
        self.asking.ask(view, machine, ask, wait, send)
    }
}

/// What the plugin asked ThinkTerm and waits for, by the id it was asked
/// with. An answer is taken in on the thread reading the input, so one that
/// comes while the plugin's own thread is busy still reaches its asker.
#[derive(Debug, Default)]
struct Asking {
    last: AtomicU64,
    waiting: Mutex<HashMap<u64, SyncSender<Answer>>>,
}

impl Asking {
    fn ask(
        &self,
        view: u64,
        machine: &str,
        ask: &Ask,
        wait: Duration,
        out: impl FnOnce(&FromPlugin) -> io::Result<()>,
    ) -> Answer {
        let id = self.last.fetch_add(1, Ordering::Relaxed) + 1;
        let (answered, answer) = mpsc::sync_channel(1);
        self.waiting().insert(id, answered);
        let asked = out(&FromPlugin::Ask {
            view,
            id,
            machine: machine.to_string(),
            ask: Raw::new(ask),
        });
        let answer = match asked {
            Ok(()) => answer
                .recv_timeout(wait)
                .map_err(|_| Answer::failed(format!("ThinkTerm did not answer within {wait:?}"))),
            Err(err) => Err(Answer::failed(format!("cannot ask ThinkTerm: {err}"))),
        };
        self.waiting().remove(&id);
        answer.unwrap_or_else(|failed| failed)
    }

    /// Hands `answer` to whoever asked `id`, if anyone still waits.
    fn answer(&self, id: u64, answer: Answer) {
        if let Some(asker) = self.waiting().remove(&id) {
            let _ = asker.try_send(answer);
        }
    }

    /// Tells the asker of `line`, an answer this SDK cannot read -- from a
    /// newer ThinkTerm, say -- that it failed. False for a line that is no
    /// answer.
    fn unreadable(&self, line: &[u8], err: &serde_json::Error) -> bool {
        let Ok(message) = serde_json::from_slice::<Value>(line) else {
            return false;
        };
        if message.get("type").and_then(Value::as_str) != Some("answer") {
            return false;
        }
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            let why = format!("ThinkTerm answered in a way this plugin does not know: {err}");
            self.answer(id, Answer::failed(why));
        }
        true
    }

    fn waiting(&self) -> std::sync::MutexGuard<'_, HashMap<u64, SyncSender<Answer>>> {
        self.waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn send(message: &FromPlugin) -> io::Result<()> {
    protocol::write_message(&mut io::stdout().lock(), message)
}

/// The directory ThinkTerm made for the plugin to keep its files in.
pub fn data_dir() -> Option<PathBuf> {
    std::env::var_os("THINKTERM_PLUGIN_DATA").map(PathBuf::from)
}

/// Serves `plugin` over standard input and output until ThinkTerm asks it
/// to stop or closes its input, which is when the program is to exit.
/// Standard output belongs to the protocol: print with `eprintln!`, which
/// goes to ThinkTerm's plugin log.
pub fn run(plugin: impl Plugin) -> io::Result<()> {
    run_with(|_| plugin)
}

/// [`run`], for a plugin that sends events or redraws of its own accord:
/// `make` is handed what does that.
pub fn run_with<P: Plugin>(make: impl FnOnce(Emitter) -> P) -> io::Result<()> {
    let (wake, woken) = mpsc::channel();
    let redrawing = Arc::new(AtomicBool::new(false));
    let asking = Arc::new(Asking::default());
    let mut plugin = make(Emitter {
        wake: wake.clone(),
        redrawing: Arc::clone(&redrawing),
        asking: Arc::clone(&asking),
    });
    let api = speaking(std::env::var("THINKTERM_PLUGIN_API").ok().as_deref());
    send(&FromPlugin::Ready { api })?;
    // Input is read on a thread of its own, so that a redraw asked for
    // from elsewhere does not wait for the next line, nor an answer for the
    // plugin's thread to be free.
    let answers = Arc::clone(&asking);
    std::thread::Builder::new()
        .name("plugin-input".into())
        .spawn(move || {
            let mut input = io::stdin().lock();
            let mut line = Vec::new();
            let end = loop {
                match protocol::read_line(&mut input, &mut line) {
                    Ok(true) => {
                        let woke = match serde_json::from_slice::<ToPlugin>(&line) {
                            Ok(ToPlugin::Answer { id, answer, .. }) => {
                                answers.answer(id, answer);
                                continue;
                            }
                            Ok(message) => Wake::Message(message),
                            Err(err) if answers.unreadable(&line, &err) => continue,
                            // Left for the plugin's thread to say it does
                            // not know.
                            Err(_) => Wake::Line(std::mem::take(&mut line)),
                        };
                        if wake.send(woke).is_err() {
                            return;
                        }
                    }
                    Ok(false) => break Ok(()),
                    Err(err) => break Err(err),
                }
            };
            let _ = wake.send(Wake::End(end));
        })?;
    Runner::new(redrawing, asking).serve(&mut plugin, bursts(woken), &mut |message| send(message))
}

/// The plugin API to say `ready` with: this SDK's, or the one ThinkTerm
/// says it speaks (`THINKTERM_PLUGIN_API`) when that is older, since a
/// ThinkTerm takes none newer than its own. What this SDK sends reads the
/// same in every version it speaks.
fn speaking(host: Option<&str>) -> u32 {
    host.and_then(|api| api.trim().parse::<u32>().ok())
        .map_or(protocol::API, |api| api.clamp(1, protocol::API))
}

/// The most taken in one burst: whatever is drawn waits no longer than this
/// many messages.
const BURST_LIMIT: usize = 64;

/// What wakes the plugin's thread, a burst at a time: the next to come,
/// and what is waiting behind it already. A panel is drawn once a burst is
/// through, so a window being resized, which sends a size for each step,
/// has it drawn for the size it stopped at, not for each.
fn bursts(woken: Receiver<Wake>) -> impl Iterator<Item = Vec<Wake>> {
    std::iter::from_fn(move || {
        let mut burst = vec![woken.recv().ok()?];
        burst.extend(woken.try_iter().take(BURST_LIMIT - 1));
        Some(burst)
    })
}

/// What the plugin's thread wakes for.
#[derive(Debug)]
enum Wake {
    Message(ToPlugin),
    /// A line that is not a message this plugin knows, or not read yet.
    Line(Vec<u8>),
    Redraw,
    /// The input ended, or broke.
    End(io::Result<()>),
}

/// A view, and the last frame sent for it.
struct Shown {
    view: View,
    last: Option<Raw>,
    /// To be drawn once the burst is through.
    dirty: bool,
}

struct Runner {
    views: BTreeMap<u64, Shown>,
    redrawing: Arc<AtomicBool>,
    asking: Arc<Asking>,
}

impl Runner {
    fn new(redrawing: Arc<AtomicBool>, asking: Arc<Asking>) -> Self {
        Self {
            views: BTreeMap::new(),
            redrawing,
            asking,
        }
    }

    /// Answers what comes in until the input ends or says stop, drawing
    /// what is to be drawn after each burst.
    fn serve<P: Plugin>(
        &mut self,
        plugin: &mut P,
        woken: impl IntoIterator<Item = Vec<Wake>>,
        out: &mut impl FnMut(&FromPlugin) -> io::Result<()>,
    ) -> io::Result<()> {
        for burst in woken {
            for wake in burst {
                if let Some(end) = self.take(plugin, wake, out)? {
                    return end;
                }
            }
            self.draw_dirty(plugin, out)?;
        }
        Ok(())
    }

    /// Takes in one wake; `Some` with how the plugin ends when it is to.
    fn take<P: Plugin>(
        &mut self,
        plugin: &mut P,
        wake: Wake,
        out: &mut impl FnMut(&FromPlugin) -> io::Result<()>,
    ) -> io::Result<Option<io::Result<()>>> {
        let line = match wake {
            Wake::Message(message) => return self.handle(plugin, message, out),
            Wake::Line(line) => line,
            Wake::Redraw => {
                self.redrawing.store(false, Ordering::Release);
                self.dirty_all();
                return Ok(None);
            }
            Wake::End(end) => return Ok(Some(end)),
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        let message: ToPlugin = match serde_json::from_slice(&line) {
            Ok(message) => message,
            Err(err) => {
                if self.asking.unreadable(&line, &err) {
                    return Ok(None);
                }
                // One from a newer ThinkTerm, say. A call's caller is told;
                // anything else is let be, for an error answers a call.
                let call = serde_json::from_slice::<Value>(&line)
                    .ok()
                    .filter(|message| message.get("type").and_then(Value::as_str) == Some("call"))
                    .and_then(|message| message.get("id")?.as_u64());
                if let Some(id) = call {
                    out(&FromPlugin::Error {
                        id,
                        message: format!("not a message this plugin knows: {err}"),
                    })?;
                }
                return Ok(None);
            }
        };
        self.handle(plugin, message, out)
    }

    /// Answers one message; `Some` with how the plugin ends when it is to.
    fn handle<P: Plugin>(
        &mut self,
        plugin: &mut P,
        message: ToPlugin,
        out: &mut impl FnMut(&FromPlugin) -> io::Result<()>,
    ) -> io::Result<Option<io::Result<()>>> {
        match message {
            ToPlugin::Call { id, body } => {
                let mut cx = Cx::new();
                let answer = guard(|| plugin.call(body, &mut cx));
                if answer_with(out, id, answer, cx)? {
                    self.dirty_all();
                }
            }
            ToPlugin::Open { view, env, extends } => {
                // The panel an extended view extends knows it by.
                if let Some(panel) = extends.and_then(|panel| self.views.get_mut(&panel)) {
                    panel.view.extension = Some(view);
                    panel.dirty = true;
                }
                let shown = Shown {
                    view: View {
                        extends,
                        ..View::new(view, env)
                    },
                    last: None,
                    dirty: true,
                };
                self.views.insert(view, shown);
            }
            ToPlugin::Env { view, env } => {
                if let Some(shown) = self.views.get_mut(&view) {
                    shown.view.env = env;
                    shown.dirty = true;
                }
            }
            ToPlugin::Input { view, input } => {
                let Some(shown) = self.views.get_mut(&view) else {
                    return Ok(None);
                };
                match &input {
                    Input::Focus(focus) => shown.view.focus = Some(focus.clone()),
                    Input::Blur => shown.view.focus = None,
                    _ => {}
                }
                let shown = &self.views[&view];
                let panel = shown.view.panel();
                let mut cx = Cx::new();
                let handled = guard(|| {
                    plugin.input(&shown.view, input, &mut cx);
                    Ok(())
                });
                if let Err(err) = handled {
                    eprintln!("input in panel {view}: {err:#}");
                }
                // What one of a panel's views shows, the other may follow.
                self.dirty_panel(panel);
                let effects = cx.finish();
                for body in effects.events {
                    out(&FromPlugin::Event { body })?;
                }
                if effects.redraw {
                    self.dirty_all();
                }
            }
            ToPlugin::Rows { view, wanted } => {
                // Rows come from a plugin that has drawn the view, and made
                // whatever it keeps for one: asked for before that, on an
                // opening, the view is drawn first.
                if self
                    .views
                    .get(&view)
                    .is_some_and(|shown| shown.last.is_none())
                {
                    self.draw(plugin, view, out)?;
                }
                let Some(shown) = self.views.get(&view) else {
                    return Ok(None);
                };
                let rows = guard(|| Ok(plugin.rows(&shown.view, &wanted)));
                let mut rows = rows.unwrap_or_else(|err| {
                    eprintln!("rows of panel {view}: {err:#}");
                    Vec::new()
                });
                rows.truncate(wanted.to.saturating_sub(wanted.from) as usize);
                let rows = Rows {
                    list: wanted.list,
                    key: wanted.key,
                    version: wanted.version,
                    layout: wanted.layout,
                    from: wanted.from,
                    rows,
                };
                out(&FromPlugin::Rows {
                    view,
                    rows: Raw::new(&rows),
                })?;
            }
            ToPlugin::Close { view } => {
                // A panel's extended view goes first: ThinkTerm closes it
                // so, and it is done here for one that does not.
                let extensions: Vec<u64> = self
                    .views
                    .values()
                    .filter(|shown| shown.view.extends == Some(view))
                    .map(|shown| shown.view.id)
                    .collect();
                for extension in extensions {
                    self.close(plugin, extension);
                }
                self.close(plugin, view);
            }
            ToPlugin::Answer { id, answer, .. } => self.asking.answer(id, answer),
            ToPlugin::Stop => return Ok(Some(Ok(()))),
        }
        Ok(None)
    }

    /// Lets view `view` go, telling the plugin; the panel it extended is
    /// drawn again without it.
    fn close<P: Plugin>(&mut self, plugin: &mut P, view: u64) {
        let Some(shown) = self.views.remove(&view) else {
            return;
        };
        if let Some(panel) = shown
            .view
            .extends
            .and_then(|panel| self.views.get_mut(&panel))
            .filter(|panel| panel.view.extension == Some(view))
        {
            panel.view.extension = None;
            panel.dirty = true;
        }
        let _ = guard(|| {
            plugin.closed(&shown.view);
            Ok(())
        });
    }

    fn dirty_all(&mut self) {
        for shown in self.views.values_mut() {
            shown.dirty = true;
        }
    }

    /// Panel `panel` and its extended view are to be drawn again.
    fn dirty_panel(&mut self, panel: u64) {
        for shown in self.views.values_mut() {
            if shown.view.panel() == panel {
                shown.dirty = true;
            }
        }
    }

    fn draw_dirty<P: Plugin>(
        &mut self,
        plugin: &mut P,
        out: &mut impl FnMut(&FromPlugin) -> io::Result<()>,
    ) -> io::Result<()> {
        let views: Vec<u64> = self
            .views
            .iter()
            .filter(|(_, shown)| shown.dirty)
            .map(|(view, _)| *view)
            .collect();
        for view in views {
            self.draw(plugin, view, out)?;
        }
        Ok(())
    }

    /// Draws `view`, and sends the frame unless it is the one sent last.
    fn draw<P: Plugin>(
        &mut self,
        plugin: &mut P,
        view: u64,
        out: &mut impl FnMut(&FromPlugin) -> io::Result<()>,
    ) -> io::Result<()> {
        let Some(shown) = self.views.get_mut(&view) else {
            return Ok(());
        };
        shown.dirty = false;
        let mut frame = Frame::default();
        let drawn = guard(|| {
            plugin.draw(&shown.view, &mut frame);
            Ok(())
        });
        if let Err(err) = drawn {
            // What was on show stays.
            eprintln!("drawing panel {view}: {err:#}");
            return Ok(());
        }
        let frame = Raw::new(&frame);
        if shown.last.as_ref() == Some(&frame) {
            return Ok(());
        }
        shown.last = Some(frame.clone());
        out(&FromPlugin::Frame { view, frame })
    }
}

/// A plugin that panics answers with an error and goes on serving; the
/// panic itself is on standard error, in the log.
fn guard<T>(work: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    match std::panic::catch_unwind(AssertUnwindSafe(work)) {
        Ok(result) => result,
        Err(panic) => {
            let why = panic
                .downcast_ref::<&str>()
                .map(|why| why.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            Err(anyhow!("the plugin panicked: {why}"))
        }
    }
}

/// Sends the answer and the events after it; true when the panels are to
/// be drawn again.
fn answer_with(
    out: &mut impl FnMut(&FromPlugin) -> io::Result<()>,
    id: u64,
    answer: anyhow::Result<Value>,
    cx: Cx,
) -> io::Result<bool> {
    let effects = cx.finish();
    match answer {
        Ok(body) => out(&FromPlugin::Ok {
            id,
            body,
            watch: effects.watch,
        })?,
        Err(err) => out(&FromPlugin::Error {
            id,
            message: format!("{err:#}"),
        })?,
    }
    for body in effects.events {
        out(&FromPlugin::Event { body })?;
    }
    Ok(effects.redraw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use panel::{Click, Hit, List, Text};
    use serde_json::json;

    /// Counts calls, lists as a watcher, and draws the count in a panel
    /// with a list of that many rows.
    #[derive(Default)]
    struct Counter {
        calls: u64,
        closed: Vec<u64>,
    }

    impl Plugin for Counter {
        fn call(&mut self, body: Value, cx: &mut Cx) -> anyhow::Result<Value> {
            match body["op"].as_str() {
                Some("add") => {
                    self.calls += 1;
                    cx.emit(json!({"event": "changed"}));
                    cx.redraw();
                    Ok(json!(self.calls))
                }
                Some("get") => {
                    cx.watch();
                    Ok(json!(self.calls))
                }
                Some("boom") => panic!("boom"),
                _ => anyhow::bail!("unknown op"),
            }
        }

        fn draw(&mut self, view: &View, frame: &mut Frame) {
            frame.push(Text::new(
                0.0,
                0.0,
                view.env.width,
                20.0,
                self.calls.to_string(),
            ));
            frame.push(Hit::new("add", 0.0, 0.0, view.env.width, 20.0));
            let key = self.calls.to_string();
            frame.push(List::new(
                "rows",
                0.0,
                20.0,
                view.env.width,
                100.0,
                1000,
                20.0,
                key,
            ));
        }

        fn rows(&mut self, _view: &View, wanted: &RowsWanted) -> Vec<Vec<Item>> {
            (wanted.from..wanted.to + 5)
                .map(|row| vec![Text::new(0.0, 0.0, 10.0, 20.0, row.to_string()).into()])
                .collect()
        }

        fn input(&mut self, _view: &View, input: Input, _cx: &mut Cx) {
            if matches!(input, Input::Click(Click { id, .. }) if id == "add") {
                self.calls += 1;
            }
        }

        fn closed(&mut self, view: &View) {
            self.closed.push(view.id);
        }
    }

    fn env() -> Value {
        json!({
            "width": 300, "height": 500, "scale": 2, "dark": true,
            "small": {"size": 11, "line": 15},
            "body": {"size": 13, "line": 18},
            "title": {"size": 15, "line": 20},
            "mono": {"size": 12, "line": 17, "advance": 7}
        })
    }

    /// Serves `woken` one at a time, each a burst of its own.
    fn serve_all<P: Plugin>(plugin: &mut P, woken: Vec<Wake>) -> Vec<Value> {
        serve_bursts(plugin, woken.into_iter().map(|wake| vec![wake]).collect())
    }

    fn serve_bursts<P: Plugin>(plugin: &mut P, bursts: Vec<Vec<Wake>>) -> Vec<Value> {
        let mut sent = Vec::new();
        Runner::new(Arc::default(), Arc::default())
            .serve(plugin, bursts, &mut |message| {
                sent.push(serde_json::to_value(message).unwrap());
                Ok(())
            })
            .unwrap();
        sent
    }

    fn lines(messages: &[Value]) -> Vec<Wake> {
        messages
            .iter()
            .map(|message| Wake::Line(serde_json::to_vec(message).unwrap()))
            .collect()
    }

    fn serve_lines(input: &str) -> Vec<Value> {
        let woken = input
            .lines()
            .map(|line| Wake::Line(line.as_bytes().to_vec()))
            .collect();
        serve_all(&mut Counter::default(), woken)
    }

    #[test]
    fn an_ask_waits_for_its_own_answer_and_no_longer_than_it_may() {
        let asking = Arc::new(Asking::default());
        let ask = Ask::Stat {
            path: "/home/user/a".into(),
        };
        // ThinkTerm answers from the thread reading the input: here, one
        // that sees what was sent.
        let (sent, asked) = mpsc::channel();
        let answering = {
            let asking = Arc::clone(&asking);
            std::thread::spawn(move || {
                let (view, id) = asked.recv().unwrap();
                asking.answer(id + 1, Answer::failed("not this one"));
                asking.answer(id, Answer::Stat { entry: None });
                view
            })
        };
        let answer = asking.ask(3, "m1", &ask, Duration::from_secs(10), |message| {
            let FromPlugin::Ask {
                view,
                id,
                machine,
                ask,
            } = message
            else {
                panic!("{message:?}")
            };
            assert_eq!(machine, "m1", "names the machine it is for");
            assert_eq!(
                ask.read::<Ask>().unwrap(),
                Ask::Stat {
                    path: "/home/user/a".into()
                }
            );
            sent.send((*view, *id)).unwrap();
            Ok(())
        });
        assert_eq!(answer, Answer::Stat { entry: None });
        assert_eq!(answering.join().unwrap(), 3, "asked for the panel it names");
        assert!(asking.waiting().is_empty());

        let late = asking.ask(3, "m1", &ask, Duration::from_millis(20), |_| Ok(()));
        assert!(
            matches!(late, Answer::Failed { ref why, .. } if why.contains("did not answer")),
            "{late:?}"
        );
        assert!(asking.waiting().is_empty(), "given up on, and let go");
        // An answer after that finds nobody, and is dropped.
        asking.answer(2, Answer::Stat { entry: None });
    }

    #[test]
    fn an_answer_among_the_lines_reaches_its_asker_not_the_plugin() {
        let asking = Arc::new(Asking::default());
        let (answered, answer) = mpsc::sync_channel(1);
        asking.waiting().insert(7, answered);
        let line = json!({"type": "answer", "view": 1, "id": 7, "answer": {"result": "failed", "why": "gone"}});
        let mut sent = Vec::new();
        Runner::new(Arc::default(), Arc::clone(&asking))
            .serve(
                &mut Counter::default(),
                vec![lines(&[line])],
                &mut |message| {
                    sent.push(serde_json::to_value(message).unwrap());
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(answer.try_recv().unwrap(), Answer::failed("gone"));
        assert!(sent.is_empty(), "nothing to say about it: {sent:?}");
    }

    #[test]
    fn an_answer_it_cannot_read_still_reaches_its_asker_and_no_call() {
        let asking = Arc::new(Asking::default());
        let (answered, answer) = mpsc::sync_channel(1);
        asking.waiting().insert(1, answered);
        // A newer ThinkTerm's answer, and a message this SDK does not know
        // that carries an id: neither is a call to answer with an error.
        let lines = lines(&[
            json!({"type": "answer", "view": 1, "id": 1, "answer": {"result": "someday"}}),
            json!({"type": "someday", "id": 2}),
            json!({"type": "call", "id": 3, "body": {"op": "get"}, "extra": 1}),
        ]);
        let mut sent = Vec::new();
        Runner::new(Arc::default(), Arc::clone(&asking))
            .serve(&mut Counter::default(), vec![lines], &mut |message| {
                sent.push(serde_json::to_value(message).unwrap());
                Ok(())
            })
            .unwrap();
        let Answer::Failed { why, .. } = answer.try_recv().unwrap() else {
            panic!("the asker is told")
        };
        assert!(why.contains("does not know"), "{why}");
        assert_eq!(
            sent,
            [json!({"type": "ok", "id": 3, "body": 0, "watch": true})],
            "only the call is answered"
        );
    }

    #[test]
    fn calls_are_answered_by_id_with_their_events_after() {
        let sent = serve_lines(concat!(
            "{\"type\":\"call\",\"id\":1,\"body\":{\"op\":\"get\"}}\n",
            "\n",
            "{\"type\":\"call\",\"id\":2,\"body\":{\"op\":\"add\"}}\n",
            "{\"type\":\"call\",\"id\":3,\"body\":{\"op\":\"nope\"}}\n",
        ));
        assert_eq!(
            sent,
            [
                json!({"type": "ok", "id": 1, "body": 0, "watch": true}),
                json!({"type": "ok", "id": 2, "body": 1}),
                json!({"type": "event", "body": {"event": "changed"}}),
                json!({"type": "error", "id": 3, "message": "unknown op"}),
            ]
        );
    }

    #[test]
    fn it_stops_when_told_and_outlives_a_panic_and_the_unknown() {
        let sent = serve_lines(concat!(
            "{\"type\":\"call\",\"id\":1,\"body\":{\"op\":\"boom\"}}\n",
            "{\"type\":\"command\",\"id\":2,\"command\":\"decode\"}\n",
            "{\"type\":\"frobnicate\"}\n",
            "not json\n",
            "{\"type\":\"stop\"}\n",
            "{\"type\":\"call\",\"id\":3,\"body\":{\"op\":\"get\"}}\n",
        ));
        // A kind of message it does not know is let be, id or not: an
        // error answers a call.
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0]["id"], 1);
        assert!(sent[0]["message"]
            .as_str()
            .unwrap()
            .contains("panicked: boom"));
    }

    #[test]
    fn a_panel_is_drawn_when_it_opens_after_input_and_when_asked() {
        let mut plugin = Counter::default();
        let click = json!({"click": {"id": "add", "x": 1, "y": 1}});
        let mut woken = lines(&[
            json!({"type": "open", "view": 7, "env": env()}),
            json!({"type": "rows", "view": 7, "wanted": {"list": "rows", "key": "0", "from": 0, "to": 3}}),
            json!({"type": "input", "view": 7, "input": click}),
            // Drawn the same as before: not sent again.
            json!({"type": "env", "view": 7, "env": env()}),
        ]);
        woken.push(Wake::Redraw);
        woken.extend(lines(&[
            json!({"type": "call", "id": 1, "body": {"op": "add"}}),
            json!({"type": "close", "view": 7}),
            json!({"type": "input", "view": 7, "input": click}),
        ]));
        woken.push(Wake::End(Ok(())));
        let sent = serve_all(&mut plugin, woken);
        let kinds: Vec<&str> = sent.iter().map(|m| m["type"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            ["frame", "rows", "frame", "ok", "event", "frame"],
            "{sent:?}"
        );
        assert_eq!(sent[0]["view"], 7);
        assert_eq!(sent[0]["frame"]["items"][0]["text"]["text"], "0");
        let rows = &sent[1]["rows"];
        assert_eq!(rows["from"], 0);
        assert_eq!(
            rows["rows"].as_array().unwrap().len(),
            3,
            "no more than asked for"
        );
        assert_eq!(
            sent[2]["frame"]["items"][0]["text"]["text"], "1",
            "the click counted"
        );
        assert_eq!(
            sent[5]["frame"]["items"][0]["text"]["text"], "2",
            "the call redrew"
        );
        assert_eq!(plugin.closed, [7]);
        assert_eq!(plugin.calls, 2, "no input reaches a closed view");
    }

    #[test]
    fn a_burst_is_drawn_once_it_is_through_and_rows_are_answered_at_once() {
        let mut plugin = Counter::default();
        let sized = |width: u32| {
            let mut env = env();
            env["width"] = json!(width);
            json!({"type": "env", "view": 7, "env": env})
        };
        let wanted =
            json!({"list": "rows", "key": "0", "version": 3, "layout": 2, "from": 0, "to": 2});
        let sent = serve_bursts(
            &mut plugin,
            vec![
                lines(&[json!({"type": "open", "view": 7, "env": env()})]),
                lines(&[
                    sized(310),
                    sized(320),
                    json!({"type": "rows", "view": 7, "wanted": wanted}),
                    sized(330),
                ]),
            ],
        );
        let kinds: Vec<&str> = sent.iter().map(|m| m["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["frame", "rows", "frame"], "{sent:?}");
        assert_eq!(
            sent[2]["frame"]["items"][0]["text"]["w"], 330.0,
            "drawn for the size it stopped at"
        );
        let rows = &sent[1]["rows"];
        assert_eq!((&rows["version"], &rows["layout"]), (&json!(3), &json!(2)));
    }

    /// Says in each view which it is and how many clicks it has had, and
    /// asks for an extended view from its panel.
    #[derive(Default)]
    struct Extending {
        clicks: u32,
        closed: Vec<u64>,
    }

    impl Plugin for Extending {
        fn draw(&mut self, view: &View, frame: &mut Frame) {
            let which = match (view.extended(), view.extension) {
                (true, _) => format!("extends {}", view.panel()),
                (false, Some(extension)) => format!("extended by {extension}"),
                (false, None) => "alone".to_string(),
            };
            let said = format!("{which}, {} clicks", self.clicks);
            frame.push(Text::new(0.0, 0.0, 10.0, 10.0, said));
            frame.extend(!view.extended());
        }

        fn input(&mut self, _view: &View, _input: Input, _cx: &mut Cx) {
            self.clicks += 1;
        }

        fn closed(&mut self, view: &View) {
            self.closed.push(view.id);
        }
    }

    #[test]
    fn an_extended_view_is_drawn_with_its_panel_and_goes_before_it() {
        let mut plugin = Extending::default();
        let click = json!({"click": {"id": "x", "x": 1, "y": 1}});
        let woken = lines(&[
            json!({"type": "open", "view": 1, "env": env()}),
            json!({"type": "open", "view": 2, "env": env(), "extends": 1}),
            json!({"type": "input", "view": 2, "input": click}),
            json!({"type": "close", "view": 2}),
            json!({"type": "open", "view": 3, "env": env(), "extends": 1}),
            json!({"type": "close", "view": 1}),
        ]);
        let sent = serve_all(&mut plugin, woken);
        let drawn: Vec<(u64, String)> = sent
            .iter()
            .map(|message| {
                let text = &message["frame"]["items"][0]["text"]["text"];
                (
                    message["view"].as_u64().unwrap(),
                    text.as_str().unwrap().to_string(),
                )
            })
            .collect();
        let expected = [
            (1, "alone, 0 clicks"),
            (1, "extended by 2, 0 clicks"),
            (2, "extends 1, 0 clicks"),
            // A click in either is drawn in both.
            (1, "extended by 2, 1 clicks"),
            (2, "extends 1, 1 clicks"),
            (1, "alone, 1 clicks"),
            (1, "extended by 3, 1 clicks"),
            (3, "extends 1, 1 clicks"),
        ];
        let expected: Vec<(u64, String)> = expected
            .iter()
            .map(|(view, text)| (*view, text.to_string()))
            .collect();
        assert_eq!(drawn, expected);
        assert_eq!(sent[0]["frame"]["extend"], true, "a panel asks");
        assert!(
            sent[2]["frame"].get("extend").is_none(),
            "an extended view does not"
        );
        assert_eq!(plugin.closed, [2, 3, 1], "the extended view goes first");
    }

    /// Has rows only for a view it has drawn: what it keeps for one is made
    /// in `draw`.
    #[derive(Default)]
    struct Lazy {
        drawn: std::collections::HashSet<u64>,
    }

    impl Plugin for Lazy {
        fn draw(&mut self, view: &View, frame: &mut Frame) {
            self.drawn.insert(view.id);
            frame.push(List::new("rows", 0.0, 0.0, 100.0, 100.0, 10, 10.0, "k"));
        }

        fn rows(&mut self, view: &View, wanted: &RowsWanted) -> Vec<Vec<Item>> {
            if !self.drawn.contains(&view.id) {
                return Vec::new();
            }
            (wanted.from..wanted.to)
                .map(|_| vec![Text::new(0.0, 0.0, 1.0, 1.0, "row").into()])
                .collect()
        }
    }

    #[test]
    fn rows_asked_for_before_a_view_is_drawn_come_after_its_first_frame() {
        let wanted = json!({"list": "rows", "key": "k", "from": 0, "to": 3});
        let sent = serve_bursts(
            &mut Lazy::default(),
            vec![lines(&[
                json!({"type": "open", "view": 5, "env": env()}),
                json!({"type": "rows", "view": 5, "wanted": wanted}),
            ])],
        );
        let kinds: Vec<&str> = sent.iter().map(|m| m["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["frame", "rows"], "{sent:?}");
        assert_eq!(
            sent[1]["rows"]["rows"].as_array().unwrap().len(),
            3,
            "drawn first, so they are there"
        );
    }

    /// Keeps a field's text, and draws where the keyboard is in its view.
    #[derive(Default)]
    struct Typing {
        add: panel::FieldText,
        submitted: Vec<String>,
    }

    impl Plugin for Typing {
        fn draw(&mut self, view: &View, frame: &mut Frame) {
            let focus = match &view.focus {
                None => "away".to_string(),
                Some(Focus { id: None }) => "panel".to_string(),
                Some(Focus { id: Some(id) }) => id.clone(),
            };
            frame.push(Text::new(0.0, 0.0, 10.0, 10.0, focus));
            frame.push(self.add.field("add", 0.0, 20.0, 100.0, 24.0));
        }

        fn input(&mut self, _view: &View, input: Input, _cx: &mut Cx) {
            match input {
                Input::Text(typed) => {
                    self.add.heard(&typed);
                }
                Input::Submit(typed) => {
                    self.submitted.push(typed.text);
                    self.add.clear();
                }
                _ => {}
            }
        }
    }

    #[test]
    fn a_view_knows_where_the_keyboard_is_and_a_submit_empties_its_field() {
        let mut plugin = Typing::default();
        let sent = serve_all(
            &mut plugin,
            lines(&[
                json!({"type": "open", "view": 1, "env": env()}),
                json!({"type": "input", "view": 1, "input": {"focus": {"id": "add"}}}),
                json!({"type": "input", "view": 1, "input": {"text": {"id": "add", "text": "TSM"}}}),
                json!({"type": "input", "view": 1, "input": {"submit": {"id": "add", "text": "TSM"}}}),
                json!({"type": "input", "view": 1, "input": "blur"}),
            ]),
        );
        let drawn: Vec<(String, String, u64)> = sent
            .iter()
            .map(|message| {
                let items = &message["frame"]["items"];
                (
                    items[0]["text"]["text"].as_str().unwrap().to_string(),
                    items[1]["field"]["value"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                    items[1]["field"]["seq"].as_u64().unwrap_or(0),
                )
            })
            .collect();
        assert_eq!(
            drawn,
            [
                ("away".to_string(), String::new(), 0),
                ("add".to_string(), String::new(), 0),
                ("add".to_string(), "TSM".to_string(), 0),
                ("add".to_string(), String::new(), 1),
                ("away".to_string(), String::new(), 1),
            ]
        );
        assert_eq!(plugin.submitted, ["TSM"]);
    }

    #[test]
    fn it_says_the_api_of_a_thinkterm_older_than_itself() {
        assert_eq!(speaking(None), protocol::API);
        assert_eq!(speaking(Some("1")), 1);
        assert_eq!(speaking(Some(" 1\n")), 1);
        assert_eq!(speaking(Some("99")), protocol::API);
        assert_eq!(speaking(Some("0")), 1);
        assert_eq!(speaking(Some("two")), protocol::API);
    }
}
