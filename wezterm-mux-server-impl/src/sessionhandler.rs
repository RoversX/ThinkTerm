use crate::PKI;
use anyhow::{anyhow, Context};
use codec::*;
use config::keyassignment::{ScrollbackEraseMode, SpawnTabDomain};
use config::TermConfig;
use mux::client::ClientId;
use mux::command_spec::CommandSpecExt;
use mux::domain::SplitSource;
use mux::pane::{CachePolicy, Pane, PaneId};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::TabId;
use mux::{
    ClientRegistrationId, FrontendAccessMode as MuxFrontendAccessMode,
    FrontendAccessState as MuxFrontendAccessState, FrontendPaneViewport, FrontendViewport,
    FrontendViewportState, Mux, PaletteSessionId,
};
use promise::spawn::spawn_into_main_thread;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use termwiz::surface::SequenceNo;
use url::Url;

/// This machine's distro id, read once. Clients ask for it in the version
/// handshake so that a host reached through the mux can show the right logo
/// without anyone opening a second connection to go and look.
/// The palette the server's `color_scheme`/`colors` resolve to, or the
/// built-in default when the configuration names neither.
pub fn configured_default_palette() -> ColorPalette {
    // `resolved_palette`, as every local pane uses: the named scheme
    // with the `colors` overlay on it, or the overlay alone.
    ColorPalette::from(config::configuration().resolved_palette.clone())
}

fn local_os_release_id() -> &'static Option<String> {
    static ID: OnceLock<Option<String>> = OnceLock::new();
    ID.get_or_init(|| {
        std::fs::read_to_string("/etc/os-release")
            .ok()
            .as_deref()
            .and_then(mux::ssh::parse_os_release_id)
    })
}
use wezterm_term::color::ColorPalette;
use wezterm_term::terminal::Alert;
use wezterm_term::StableRowIndex;

lazy_static::lazy_static! {
    /// Serializes the authoritative tree decision with the live-workspace
    /// check and spawn. Without this, two clients opening the same empty
    /// Thread concurrently can each create its first tab.
    static ref THINKTERM_MATERIALIZE: futures::lock::Mutex<()> =
        futures::lock::Mutex::new(());
}

#[derive(Clone)]
pub struct PduSender {
    func: Arc<dyn Fn(DecodedPdu) -> anyhow::Result<()> + Send + Sync>,
    closed: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl PduSender {
    pub fn send(&self, pdu: DecodedPdu) -> anyhow::Result<()> {
        (self.func)(pdu)
    }

    /// Whether the connection behind this sender is gone, for work that
    /// would otherwise wait on its behalf for a long time.
    pub fn is_closed(&self) -> bool {
        (self.closed)()
    }

    pub fn new<T>(f: T) -> Self
    where
        T: Fn(DecodedPdu) -> anyhow::Result<()> + Send + Sync + 'static,
    {
        Self::with_closed(f, || false)
    }

    pub fn with_closed<T, C>(f: T, closed: C) -> Self
    where
        T: Fn(DecodedPdu) -> anyhow::Result<()> + Send + Sync + 'static,
        C: Fn() -> bool + Send + Sync + 'static,
    {
        Self {
            func: Arc::new(f),
            closed: Arc::new(closed),
        }
    }
}

#[derive(Default, Debug)]
pub(crate) struct PerPane {
    cursor_position: StableCursorPosition,
    title: String,
    working_dir: Option<Url>,
    dimensions: RenderableDimensions,
    mouse_grabbed: bool,
    alt_screen: bool,
    keyboard_encoding: WireKeyboardEncoding,
    /// Outer None means that this connection has never received application
    /// palette state for the pane. Inner None is an explicit reset to the
    /// client's own configured palette.
    last_sent_application_palette: Option<Option<ColorPalette>>,
    seqno: SequenceNo,
    pub(crate) notifications: Vec<Alert>,
    /// Images already sent for this pane, so a fetch can be answered even
    /// after the cell they were attached to has moved on.
    sent_images: crate::sent_images::SentImages,
    /// A push task exists for this pane and has not yet read the pane.
    push_scheduled: bool,
    /// Terminal input from this connection for this pane, in the order it
    /// was sent, waiting for the pane to be free. See `drain_pane_inputs`.
    inputs: VecDeque<QueuedInput>,
    /// What `inputs` weighs, against `INPUT_QUEUE_LIMIT`.
    queued_input_bytes: usize,
    /// A drain task exists for `inputs`.
    input_draining: bool,
}

/// What may wait for one pane from one connection before further input is
/// refused: a paste, not a program pouring text into a pane that is not
/// reading it. The client's own write queue stops at four megabytes;
/// pastes and `cli send-text` do not pass through it.
const INPUT_QUEUE_LIMIT: usize = 8 * 1024 * 1024;

/// One piece of terminal input a client sent for a pane.
enum PaneInput {
    Key {
        event: termwiz::input::KeyEvent,
        input_serial: InputSerial,
    },
    Paste(String),
    Mouse(wezterm_term::input::MouseEvent),
    Write(Vec<u8>),
    EraseScrollback(ScrollbackEraseMode),
}

impl PaneInput {
    /// Whether applying this needs the pane's terminal, and so a pane
    /// that is free. Bytes for the pty do not.
    fn needs_the_terminal(&self) -> bool {
        !matches!(self, PaneInput::Write(_))
    }

    /// What this costs to keep, against `INPUT_QUEUE_LIMIT`: its bytes,
    /// with a floor for the pieces that are all bookkeeping.
    fn weight(&self) -> usize {
        match self {
            PaneInput::Paste(text) => text.len().max(64),
            PaneInput::Write(data) => data.len().max(64),
            PaneInput::Key { .. } | PaneInput::Mouse(_) | PaneInput::EraseScrollback(_) => 64,
        }
    }
}

/// Input waiting in `PerPane::inputs`, with the reply its request is owed.
struct QueuedInput {
    input: PaneInput,
    respond: Box<dyn FnOnce(anyhow::Result<Pdu>) + Send>,
}

impl std::fmt::Debug for QueuedInput {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.input {
            PaneInput::Key { .. } => "key",
            PaneInput::Paste(_) => "paste",
            PaneInput::Mouse(_) => "mouse",
            PaneInput::Write(data) => return write!(fmt, "write({} bytes)", data.len()),
            PaneInput::EraseScrollback(_) => "erase scrollback",
        };
        fmt.write_str(kind)
    }
}

/// Who the input came from: what the arms used to read off the handler
/// when they applied it inline.
#[derive(Clone)]
struct InputSource {
    client_id: Option<Arc<ClientId>>,
    registration: Option<ClientRegistrationId>,
    palette_session_id: Option<PaletteSessionId>,
}

impl PerPane {
    /// Claim the one push slot; false when a push is already on its way,
    /// which will carry whatever the caller wanted sent.
    fn claim_push(&mut self) -> bool {
        !std::mem::replace(&mut self.push_scheduled, true)
    }

    /// Give the slot back. Done just before the pane is read, so output
    /// that lands during the read schedules the next push.
    fn release_push(&mut self) {
        self.push_scheduled = false;
    }

    /// Queue `item`; Ok(true) when the caller has to start the drain.
    /// Refused, and handed back, when the queue is at its limit.
    fn queue_input(&mut self, item: QueuedInput) -> Result<bool, QueuedInput> {
        let weight = item.input.weight();
        if self.queued_input_bytes + weight > INPUT_QUEUE_LIMIT {
            return Err(item);
        }
        self.queued_input_bytes += weight;
        self.inputs.push_back(item);
        Ok(!std::mem::replace(&mut self.input_draining, true))
    }

    /// The next queued piece, its weight given back.
    fn pop_input(&mut self) -> Option<QueuedInput> {
        let item = self.inputs.pop_front()?;
        self.queued_input_bytes = self.queued_input_bytes.saturating_sub(item.input.weight());
        Some(item)
    }

    fn needs_application_palette(&self, palette: &Option<ColorPalette>) -> bool {
        self.last_sent_application_palette.as_ref() != Some(palette)
    }

    fn record_application_palette(&mut self, palette: Option<ColorPalette>) {
        self.last_sent_application_palette = Some(palette);
    }

    /// The pane as this connection last described it.
    fn last_sent(&self) -> LastSent {
        LastSent {
            cursor_position: self.cursor_position,
            title: self.title.clone(),
            working_dir: self.working_dir.clone(),
            dimensions: self.dimensions,
            mouse_grabbed: self.mouse_grabbed,
            alt_screen: self.alt_screen,
            keyboard_encoding: self.keyboard_encoding,
            seqno: self.seqno,
        }
    }

    /// Record what `read_pane_changes` found, and build the push for it.
    fn commit_changes(
        &mut self,
        pane_id: PaneId,
        reading: PaneReading,
    ) -> GetPaneRenderChangesResponse {
        let PaneReading {
            mouse_grabbed,
            alt_screen,
            keyboard_encoding,
            dimensions,
            cursor_position,
            title,
            working_dir,
            seqno,
            dirty_lines,
            bonus_lines,
            input_serial,
        } = reading;
        self.cursor_position = cursor_position;
        self.title = title.clone();
        self.working_dir = working_dir.clone();
        self.dimensions = dimensions;
        self.mouse_grabbed = mouse_grabbed;
        self.alt_screen = alt_screen;
        self.keyboard_encoding = keyboard_encoding;
        self.seqno = seqno;

        self.sent_images.remember(&bonus_lines);
        let bonus_lines = bonus_lines.into();
        GetPaneRenderChangesResponse {
            pane_id,
            mouse_grabbed,
            alt_screen,
            keyboard_encoding,
            dirty_lines: dirty_lines.iter().cloned().collect(),
            dimensions,
            cursor_position,
            title,
            bonus_lines,
            working_dir: working_dir.map(Into::into),
            input_serial,
            seqno,
        }
    }
}

/// The pane as a connection last described it: what a reading of the pane
/// is held against. Copied out of `PerPane` so that the reading is made
/// with no `PerPane` lock held. Reading the pane waits on its terminal
/// lock, which its parser holds for a whole batch of output, and the
/// connection's thread locks `PerPane` to schedule the pane's next push:
/// with the lock held across the reading, that thread -- and every client
/// it serves -- waited behind this one pane's parser, which is the wait
/// the connection threads exist to avoid.
#[derive(Clone)]
struct LastSent {
    cursor_position: StableCursorPosition,
    title: String,
    working_dir: Option<Url>,
    dimensions: RenderableDimensions,
    mouse_grabbed: bool,
    alt_screen: bool,
    keyboard_encoding: WireKeyboardEncoding,
    seqno: SequenceNo,
}

/// What a reading of the pane found, for `PerPane::commit_changes`.
struct PaneReading {
    mouse_grabbed: bool,
    alt_screen: bool,
    keyboard_encoding: WireKeyboardEncoding,
    dimensions: RenderableDimensions,
    cursor_position: StableCursorPosition,
    title: String,
    working_dir: Option<Url>,
    seqno: SequenceNo,
    dirty_lines: rangeset::RangeSet<StableRowIndex>,
    bonus_lines: Vec<(StableRowIndex, wezterm_term::Line)>,
    input_serial: Option<InputSerial>,
}

/// Select from owned rows after the pane's terminal lock has been released.
/// Keep dirty-set traversal, mutation and compression out of that lock:
/// under full-screen output these do not save any row copies and would
/// otherwise extend the time the parser has to wait for its terminal.
fn select_dirty_lines(
    first: StableRowIndex,
    lines: Vec<wezterm_term::Line>,
    dirty: &mut rangeset::RangeSet<StableRowIndex>,
) -> Vec<(StableRowIndex, wezterm_term::Line)> {
    let end = first + lines.len() as StableRowIndex;
    let selected = lines
        .into_iter()
        .enumerate()
        .filter_map(|(idx, mut line)| {
            let row = first + idx as StableRowIndex;
            if dirty.contains(row) {
                line.compress_for_scrollback();
                Some((row, line))
            } else {
                None
            }
        })
        .collect();
    // All dirty rows in the actual returned range were copied. Remove
    // the range once instead of allocating bookkeeping for each row.
    if first < end {
        dirty.remove_range(first..end);
    }
    selected
}

/// Read `pane` against what was `last` sent: None when nothing changed and
/// nothing forces a push. Takes the pane's terminal lock, several times;
/// no `PerPane` lock may be held by the caller.
fn read_pane_changes(
    pane: &Arc<dyn Pane>,
    last: &LastSent,
    force_with_input_serial: Option<InputSerial>,
) -> Option<PaneReading> {
    let mut changed = false;
    let mouse_grabbed = pane.is_mouse_grabbed();
    if mouse_grabbed != last.mouse_grabbed {
        changed = true;
    }

    let alt_screen = pane.is_alt_screen_active();
    if alt_screen != last.alt_screen {
        changed = true;
    }

    let keyboard_encoding: WireKeyboardEncoding = pane.get_keyboard_encoding().into();
    if keyboard_encoding != last.keyboard_encoding {
        changed = true;
    }

    let dims = pane.get_dimensions();
    if dims != last.dimensions {
        changed = true;
    }

    let cursor_position = pane.get_cursor_position();
    if cursor_position != last.cursor_position {
        changed = true;
    }

    let title = pane.get_title();
    if title != last.title {
        changed = true;
    }

    let working_dir = pane.get_current_working_dir(CachePolicy::AllowStale);
    if working_dir != last.working_dir {
        changed = true;
    }

    let seqno = pane.get_current_seqno();
    let mut all_dirty_lines = pane.get_changed_since(
        0..dims.physical_top + dims.viewport_rows as StableRowIndex,
        last.seqno,
    );
    if !all_dirty_lines.is_empty() {
        changed = true;
    }

    if !changed && !force_with_input_serial.is_some() {
        return None;
    }

    // Figure out what we're going to send as dirty lines vs bonus lines
    let viewport_range =
        dims.physical_top..dims.physical_top + dims.viewport_rows as StableRowIndex;

    let (first_line, lines) = pane.get_lines(viewport_range);
    let mut bonus_lines = select_dirty_lines(first_line, lines, &mut all_dirty_lines);

    // Always send the cursor's row, as that tends to the busiest and we don't
    // have a sequencing concept for our idea of the remote state.
    let (cursor_line_idx, mut lines) = pane.get_lines(cursor_position.y..cursor_position.y + 1);
    let mut cursor_line = lines.remove(0);
    cursor_line.compress_for_scrollback();
    bonus_lines.push((cursor_line_idx, cursor_line));

    Some(PaneReading {
        mouse_grabbed,
        alt_screen,
        keyboard_encoding,
        dimensions: dims,
        cursor_position,
        title,
        working_dir,
        seqno,
        dirty_lines: all_dirty_lines,
        bonus_lines,
        input_serial: force_with_input_serial,
    })
}

/// How long a push waits for a busy pane before looking again: short at
/// first, since a batch of ordinary output is applied in well under a
/// millisecond, backing off so a pane stuck for seconds is not polled at
/// full tilt. After `PUSH_DEFERRAL_LIMIT` of nothing but contention the
/// push gives up; the pane's next output schedules a fresh one, so a pane
/// wedged for good costs one warning rather than a task for ever.
const PUSH_RETRY_MIN: Duration = Duration::from_millis(2);
const PUSH_RETRY_MAX: Duration = Duration::from_millis(50);
const PUSH_DEFERRAL_LIMIT: Duration = Duration::from_secs(30);

/// The one push slot of a pane, given back however the push ends: a task
/// dropped without running, a bail, a panic -- a slot left taken would
/// stop the pane from ever pushing to this connection again.
struct PushSlot {
    per_pane: Arc<Mutex<PerPane>>,
    released: bool,
}

impl PushSlot {
    fn release(&mut self) {
        if !self.released {
            self.released = true;
            if let Ok(mut per_pane) = self.per_pane.lock() {
                per_pane.release_push();
            }
        }
    }
}

impl Drop for PushSlot {
    fn drop(&mut self) {
        self.release();
    }
}

/// Push `pane_id`'s changes to this connection once its render state can
/// be read without waiting.
///
/// Pushes run on the main thread with the rest of the mux's work, while
/// each pane's parser holds the pane's terminal lock for as long as a
/// batch of output takes to apply. Reading here while a pane was busy
/// with, say, a very large image stalled every other pane's pushes and
/// every request that needs the main thread until that batch was done --
/// with a runaway pane, for good. So the lock is probed first, and a busy
/// pane's push is put off: that pane's own output is what waits, nobody
/// else's. The probe is a moment's glance, not a reservation; a pane that
/// takes the lock back right after it still has to be waited for, once.
async fn push_pane_changes_when_free(
    pane_id: PaneId,
    sender: PduSender,
    per_pane: Arc<Mutex<PerPane>>,
) -> anyhow::Result<()> {
    let mut slot = PushSlot {
        per_pane: Arc::clone(&per_pane),
        released: false,
    };
    let Some(pane) =
        pane_when_free(pane_id, &sender, "this push", Some(PUSH_DEFERRAL_LIMIT)).await?
    else {
        // Its next output will try again.
        return Ok(());
    };
    // Released before the read, so output that lands during it
    // schedules the next push rather than being carried by nobody.
    slot.release();
    let pushed_at = Instant::now();
    let pushed = maybe_push_pane_changes(&pane, sender, per_pane, None);
    metrics::histogram!("mux_server.push.latency").record(pushed_at.elapsed());
    pushed
}

/// The pane, once its render state can be read without waiting on its
/// parser. With a `limit`, None after that long of nothing but
/// contention, with a warning naming `what` gave up; without one the wait
/// is for as long as it takes, and the warning says so once. Every read
/// of a pane on the main thread goes through here, so a pane wedged in
/// its parser costs its own readers a wait and nobody else anything. The
/// probe is a moment's glance, not a reservation; a pane that takes the
/// lock back right after it still has to be waited for, once.
async fn pane_when_free(
    pane_id: PaneId,
    sender: &PduSender,
    what: &str,
    limit: Option<Duration>,
) -> anyhow::Result<Option<Arc<dyn Pane>>> {
    let mut delay = PUSH_RETRY_MIN;
    let started = Instant::now();
    let mut warned = false;
    loop {
        if sender.is_closed() {
            anyhow::bail!("connection closed while pane {pane_id} was busy");
        }
        let Some(pane) = Mux::get().get_pane(pane_id) else {
            anyhow::bail!("no such pane {pane_id}");
        };
        if !pane.render_state_is_contended() {
            return Ok(Some(pane));
        }
        if started.elapsed() > limit.unwrap_or(PUSH_DEFERRAL_LIMIT) {
            if limit.is_some() {
                log::warn!(
                    "pane {pane_id} has been busy for {:?}; giving up on {what}",
                    started.elapsed()
                );
                return Ok(None);
            }
            if !warned {
                warned = true;
                log::warn!(
                    "pane {pane_id} has been busy for {:?}; {what} waits for it",
                    started.elapsed()
                );
            }
        }
        metrics::counter!("mux_server.push.deferred").increment(1);
        smol::Timer::after(delay).await;
        delay = (delay * 2).min(PUSH_RETRY_MAX);
    }
}

/// Apply the input queued for `pane_id` by one connection, in order, each
/// piece once the pane can take it without waiting.
///
/// Every arm used to apply its input inline on the main thread, which
/// meant taking the pane's terminal lock there: a keystroke for a pane
/// whose parser was digesting a huge frame stalled every other pane's
/// push and every request that hops to main, for as long as the frame
/// took. The input waits in the pane's own queue instead, and only the
/// pane it is for waits with it. One queue per connection and pane keeps
/// a client's input in the order it sent it, which is what the shared
/// main-thread queue used to guarantee.
///
/// Input is never given up on: a keystroke that lands late is what the
/// user typed, one that is thrown away is not, and only this pane waits.
/// Bytes for the pty need no terminal at all -- kitty and win32 keys,
/// composed text, `SendString`, `cli send-text` all arrive that way --
/// and go to the pty as soon as they reach the head of the queue, so a
/// Ctrl-C still reaches a program whose output is what wedged the pane.
async fn drain_pane_inputs(
    pane_id: PaneId,
    sender: PduSender,
    per_pane: Arc<Mutex<PerPane>>,
    source: InputSource,
) {
    let mut drain = InputDrain {
        per_pane: Arc::clone(&per_pane),
        why: "the drain of this pane's input ended before it was applied",
        done: false,
    };
    loop {
        // Empty check and hand-back under the one lock: input queued
        // between the two would start its own drain, whose queue this
        // task's guard would then take for its own and fail.
        let Some(item) = drain.next() else {
            return;
        };
        let pane = if item.input.needs_the_terminal() {
            pane_when_free(pane_id, &sender, "its input", None)
                .await
                .and_then(|pane| pane.ok_or_else(|| anyhow!("pane {pane_id} is busy")))
        } else {
            Mux::get()
                .get_pane(pane_id)
                .ok_or_else(|| anyhow!("no such pane {pane_id}"))
        };
        let pane = match pane {
            Ok(pane) => pane,
            Err(err) => {
                drain.why = "the pane or the connection went away";
                (item.respond)(Err(err));
                return;
            }
        };
        let result = apply_pane_input(&pane, &sender, &per_pane, &source, item.input);
        (item.respond)(result);
        // A burst typed while the pane was busy is applied a piece per
        // turn of the main thread, not all in one.
        smol::future::yield_now().await;
    }
}

/// Ends the drain however the task ends: clears the flag so the next
/// input starts a new one, and answers whatever is still queued, since a
/// task dropped mid-way would otherwise leave those requests unanswered
/// and the flag set for good.
struct InputDrain {
    per_pane: Arc<Mutex<PerPane>>,
    why: &'static str,
    /// The queue was found empty and given back; the guard has nothing
    /// left to do, and must not touch a drain that started since.
    done: bool,
}

impl InputDrain {
    /// The next piece, or None once the queue is empty -- in which case
    /// the drain is over, decided under the same lock as the look.
    fn next(&mut self) -> Option<QueuedInput> {
        let mut per_pane = self.per_pane.lock().unwrap();
        match per_pane.pop_input() {
            Some(item) => Some(item),
            None => {
                per_pane.input_draining = false;
                self.done = true;
                None
            }
        }
    }
}

impl Drop for InputDrain {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let left = {
            let Ok(mut per_pane) = self.per_pane.lock() else {
                return;
            };
            per_pane.input_draining = false;
            per_pane.queued_input_bytes = 0;
            std::mem::take(&mut per_pane.inputs)
        };
        for item in left {
            (item.respond)(Err(anyhow!("{}", self.why)));
        }
    }
}

/// One piece of input, applied the way its arm applied it inline: under
/// the client's identity, after the viewport claim and the palette check
/// the arms made, followed by the push the arms sent. The main thread,
/// with the pane known to be free a moment ago.
fn apply_pane_input(
    pane: &Arc<dyn Pane>,
    sender: &PduSender,
    per_pane: &Arc<Mutex<PerPane>>,
    source: &InputSource,
    input: PaneInput,
) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    let _identity = mux.with_identity(source.client_id.clone());
    if let PaneInput::EraseScrollback(erase_mode) = input {
        // Checked when the request arrived; checked again now that the
        // wait for the pane may have been long enough for the terminal
        // to change hands.
        require_registered_frontend_access(&mux, source.client_id.as_ref(), source.registration)?;
        pane.erase_scrollback(erase_mode);
        return Ok(Pdu::UnitResponse(UnitResponse {}));
    }
    claim_viewport_for_pane(
        &mux,
        source.client_id.as_ref(),
        source.registration,
        pane.pane_id(),
    )?;
    activate_client_palette(&mux, pane, source.palette_session_id)?;
    let input_serial = match input {
        PaneInput::Key {
            event,
            input_serial,
        } => {
            pane.key_down(event.key, event.modifiers)?;
            // Sent back with the push so the predictive echo can tell
            // which key the cursor position answers.
            Some(input_serial)
        }
        PaneInput::Paste(text) => {
            pane.send_paste(&text)?;
            None
        }
        PaneInput::Mouse(event) => {
            // The client coalesces rapid wheel motion into a single
            // event with an accumulated amount, but the terminal
            // emits one report per event regardless of the amount;
            // replay it per notch so mouse-mode apps scroll the
            // full distance.
            use wezterm_term::MouseButton as MB;
            let notches = match event.button {
                MB::WheelUp(n) if n > 1 => Some((MB::WheelUp(1), n)),
                MB::WheelDown(n) if n > 1 => Some((MB::WheelDown(1), n)),
                MB::WheelLeft(n) if n > 1 => Some((MB::WheelLeft(1), n)),
                MB::WheelRight(n) if n > 1 => Some((MB::WheelRight(1), n)),
                _ => None,
            };
            match notches {
                Some((notch, n)) => {
                    let mut single = event;
                    single.button = notch;
                    for _ in 0..n {
                        pane.mouse_event(single.clone())?;
                    }
                }
                None => pane.mouse_event(event)?,
            }
            None
        }
        PaneInput::Write(data) => {
            pane.writer().write_all(&data)?;
            // The pane may be busy (see `PaneInput::needs_the_terminal`);
            // its push is read once it is free, like any other.
            if per_pane.lock().unwrap().claim_push() {
                spawn_into_main_thread(push_pane_changes_when_free(
                    pane.pane_id(),
                    sender.clone(),
                    Arc::clone(per_pane),
                ))
                .detach();
            }
            return Ok(Pdu::UnitResponse(UnitResponse {}));
        }
        PaneInput::EraseScrollback(_) => unreachable!("answered above"),
    };
    maybe_push_pane_changes(pane, sender.clone(), Arc::clone(per_pane), input_serial)?;
    Ok(Pdu::UnitResponse(UnitResponse {}))
}

/// Read `pane` and push what changed. Nothing of the pane is read while
/// `per_pane` is locked (see `LastSent`); this runs on the main thread,
/// where every writer of a `PerPane`'s state runs, so what is read out of
/// it and what is written back stay consistent.
fn maybe_push_pane_changes(
    pane: &Arc<dyn Pane>,
    sender: PduSender,
    per_pane: Arc<Mutex<PerPane>>,
    force_with_input_serial: Option<InputSerial>,
) -> anyhow::Result<()> {
    // Application palette provenance must arrive even when the state is
    // `None`: that explicit reset prevents a server's configured/advisory
    // palette from taking over client rendering. Send it before line changes
    // so a real application override is installed before those lines paint.
    let application_palette = pane.palette_override();
    let (needs_palette, last) = {
        let per_pane = per_pane.lock().unwrap();
        (
            per_pane.needs_application_palette(&application_palette),
            per_pane.last_sent(),
        )
    };
    if needs_palette {
        sender.send(DecodedPdu {
            pdu: Pdu::SetApplicationPalette(SetApplicationPalette {
                pane_id: pane.pane_id(),
                palette: application_palette.clone(),
            }),
            serial: 0,
        })?;
        per_pane
            .lock()
            .unwrap()
            .record_application_palette(application_palette);
    }

    let reading = read_pane_changes(pane, &last, force_with_input_serial);
    let mut per_pane = per_pane.lock().unwrap();
    if let Some(reading) = reading {
        let resp = per_pane.commit_changes(pane.pane_id(), reading);
        sender.send(DecodedPdu {
            pdu: Pdu::GetPaneRenderChangesResponse(resp),
            serial: 0,
        })?;
    }

    let notifications: Vec<Alert> = per_pane.notifications.drain(..).collect();
    for alert in notifications {
        match alert {
            // The current application state was sent (and de-duplicated)
            // above. Never forward this alert as a configured palette.
            Alert::PaletteChanged => {}
            alert => {
                sender.send(DecodedPdu {
                    pdu: Pdu::NotifyAlert(NotifyAlert {
                        pane_id: pane.pane_id(),
                        alert,
                    }),
                    serial: 0,
                })?;
            }
        }
    }
    Ok(())
}

fn apply_client_palette(pane: &Arc<dyn Pane>, palette: Option<ColorPalette>) -> anyhow::Result<()> {
    match pane.get_config() {
        Some(config) => match config.downcast_ref::<TermConfig>() {
            Some(tc) => match palette {
                Some(palette) => tc.set_client_palette(palette),
                None => tc.clear_client_palette(),
            },
            None => {
                log::error!(
                    "pane {} doesn't have TermConfig as its config; ignoring client palette update",
                    pane.pane_id()
                );
            }
        },
        None => {
            if let Some(palette) = palette {
                let config = TermConfig::new();
                config.set_client_palette(palette);
                pane.set_config(Arc::new(config));
            }
        }
    }
    Ok(())
}

pub(crate) fn codec_viewport_state(state: FrontendViewportState) -> ClientViewportState {
    ClientViewportState {
        tab_id: state.tab_id,
        owner: state.owner,
        canonical_size: state.canonical_size,
        view: state.view.map(|view| codec::ClientView {
            scroll: view
                .scroll
                .into_iter()
                .map(|(pane_id, lines_from_bottom)| codec::ClientPaneScroll {
                    pane_id,
                    lines_from_bottom,
                })
                .collect(),
        }),
        generation: state.generation,
        access: codec_access_state(state.access),
    }
}

pub(crate) fn codec_access_state(state: MuxFrontendAccessState) -> FrontendAccessState {
    FrontendAccessState {
        mode: codec_access_mode(state.mode),
        owner: state.owner,
        generation: state.generation,
    }
}

fn codec_access_mode(mode: MuxFrontendAccessMode) -> FrontendAccessMode {
    match mode {
        MuxFrontendAccessMode::TmuxLatest => FrontendAccessMode::TmuxLatest,
        MuxFrontendAccessMode::Handoff => FrontendAccessMode::Handoff,
    }
}

fn mux_access_mode(mode: FrontendAccessMode) -> MuxFrontendAccessMode {
    match mode {
        FrontendAccessMode::TmuxLatest => MuxFrontendAccessMode::TmuxLatest,
        FrontendAccessMode::Handoff => MuxFrontendAccessMode::Handoff,
    }
}

fn mux_view(view: codec::ClientView) -> mux::FrontendView {
    mux::FrontendView {
        scroll: view
            .scroll
            .into_iter()
            .map(|entry| (entry.pane_id, entry.lines_from_bottom))
            .collect(),
    }
}

fn mux_viewport(viewport: ClientViewport) -> FrontendViewport {
    match viewport {
        ClientViewport::CellGrid { size } => FrontendViewport::CellGrid { size },
        ClientViewport::Native { size, panes } => FrontendViewport::Native {
            size,
            panes: panes
                .into_iter()
                .map(|pane| FrontendPaneViewport {
                    pane_id: pane.pane_id,
                    size: pane.size,
                    frame: pane.frame,
                })
                .collect(),
        },
    }
}

fn claim_viewport_for_pane(
    mux: &Mux,
    client_id: Option<&Arc<ClientId>>,
    registration: Option<ClientRegistrationId>,
    pane_id: PaneId,
) -> anyhow::Result<TabId> {
    let client_id =
        client_id.ok_or_else(|| anyhow!("terminal input requires an identified client"))?;
    let registration = registration
        .ok_or_else(|| anyhow!("terminal input requires a live client registration"))?;
    if mux
        .registered_client_has_frontend_access(client_id, registration)
        .is_none()
    {
        anyhow::bail!("terminal input came from a superseded client connection");
    }
    let (_domain_id, _window_id, tab_id) = mux
        .resolve_pane_id(pane_id)
        .ok_or_else(|| anyhow!("no such pane {pane_id}"))?;
    if !mux.registered_client_had_tab_input(client_id, registration, tab_id) {
        match mux.registered_client_has_frontend_access(client_id, registration) {
            None => anyhow::bail!("client connection was superseded"),
            Some(false) => anyhow::bail!("terminal is being operated on another device"),
            Some(true) => anyhow::bail!("unable to acquire the frontend layout"),
        }
    }
    Ok(tab_id)
}

fn require_registered_frontend_access(
    mux: &Mux,
    client_id: Option<&Arc<ClientId>>,
    registration: Option<ClientRegistrationId>,
) -> anyhow::Result<()> {
    let client_id = client_id.ok_or_else(|| anyhow!("request requires an identified client"))?;
    let registration =
        registration.ok_or_else(|| anyhow!("request requires a live client registration"))?;
    match mux.registered_client_has_frontend_access(client_id, registration) {
        None => anyhow::bail!("client connection was superseded"),
        Some(false) => anyhow::bail!("terminal is being operated on another device"),
        Some(true) => Ok(()),
    }
}

/// Mutations which can be reached through tab/menu chrome must still obey B's
/// global owner, but checking them must not itself claim A's layout lease.
fn requires_existing_frontend_access(pdu: &Pdu) -> bool {
    matches!(
        pdu,
        Pdu::EraseScrollbackRequest(_)
            | Pdu::KillPane(_)
            | Pdu::SetPaneZoomed(_)
            | Pdu::SpawnV2(_)
            | Pdu::SplitPane(_)
            | Pdu::SpawnPaneInStack(_)
            | Pdu::ActivatePaneInStack(_)
            | Pdu::MovePaneToStack(_)
            | Pdu::MovePaneToNewTab(_)
            | Pdu::MoveTab(_)
            | Pdu::AdjustPaneSize(_)
    )
}

fn activate_client_palette(
    mux: &Mux,
    pane: &Arc<dyn Pane>,
    palette_session_id: Option<PaletteSessionId>,
) -> anyhow::Result<()> {
    if let Some(palette_session_id) = palette_session_id {
        if let Some(palette) = mux.activate_client_palette(palette_session_id, pane.pane_id()) {
            apply_client_palette(pane, Some(palette))?;
        }
    }
    Ok(())
}

fn schedule_palette_session_cleanup(session_id: PaletteSessionId, reason: &'static str) {
    let mux = Mux::get();
    if !mux.deactivate_palette_session(session_id) {
        return;
    }
    // Deactivation above closes the stale-handler gate immediately. Applying
    // fallback here, behind work already queued by this connection, closes the
    // TOCTOU where a handler had fetched its palette just before disconnect
    // and would otherwise apply it after synchronous cleanup.
    spawn_into_main_thread(async move {
        let mux = Mux::get();
        for change in mux.unregister_palette_session(session_id) {
            if let Some(pane) = mux.get_pane(change.pane_id) {
                if let Err(err) = apply_client_palette(&pane, change.palette) {
                    log::error!(
                        "applying palette fallback for pane {} after {reason}: {err:#}",
                        change.pane_id
                    );
                }
            }
        }
    })
    .detach();
}

/// Who is on the other end of a connection, as far as the accept layer
/// could tell. The unix socket and the mTLS port are the server's own
/// user by construction; a browser is whoever presented a web token.
#[derive(Debug, Clone)]
pub enum ConnectionPeer {
    /// The unix socket: a client already on this machine, running as this
    /// user.
    Local,
    /// The mTLS port: a client that presented a certificate for this user,
    /// from somewhere else. It may do what the user could -- that is what
    /// the certificate means -- but not everything a session on the machine
    /// itself may do; opening a listener the rest of the network can reach
    /// is not a thing to be able to arrange from off it.
    Tls,
    Web(WebPeer),
}

/// A browser connection admitted by `web_auth`. The identity it presents
/// in `SetClientId` is overwritten with this, so `cli list-clients` shows
/// the token that let it in rather than whatever the page claimed.
#[derive(Debug, Clone)]
pub struct WebPeer {
    pub token_id: String,
    pub label: String,
    /// The server's own user: every web session runs as that user.
    pub username: String,
    /// Fires when the token is revoked; the connection loop drops the
    /// socket the moment it does.
    pub revoked: smol::channel::Receiver<()>,
}

/// What a browser admitted by a web token may ask for: the terminal, the
/// mux and the ThinkTerm tree, exactly as a local client would. Not on
/// the list, deliberately: `GetTlsCreds` (a client certificate neither
/// expires nor revokes, so a leaked token must not become one) and the
/// token administration PDUs (a browser minting further browsers is how
/// one leaked token becomes many).
fn web_peer_may_send(pdu: &Pdu) -> bool {
    matches!(
        pdu,
        Pdu::Ping(_)
            | Pdu::Pong(_)
            | Pdu::GetCodecVersion(_)
            | Pdu::GetServerOsRelease(_)
            | Pdu::SetClientId(_)
            | Pdu::GetClientList(_)
            | Pdu::ListPanes(_)
            | Pdu::WriteToPane(_)
            | Pdu::SendKeyDown(_)
            | Pdu::SendMouseEvent(_)
            | Pdu::SendPaste(_)
            | Pdu::Resize(_)
            | Pdu::GetLines(_)
            | Pdu::GetPaneRenderChanges(_)
            | Pdu::GetPaneRenderableDimensions(_)
            | Pdu::GetImageCell(_)
            | Pdu::SearchScrollbackRequest(_)
            | Pdu::EraseScrollbackRequest(_)
            | Pdu::SetPalette(_)
            | Pdu::SetPaneZoomed(_)
            | Pdu::SplitPane(_)
            | Pdu::KillPane(_)
            | Pdu::SpawnV2(_)
            | Pdu::SetWindowWorkspace(_)
            | Pdu::RenameWorkspace(_)
            | Pdu::SetFocusedPane(_)
            | Pdu::MovePaneToNewTab(_)
            | Pdu::MoveTab(_)
            | Pdu::ActivatePaneDirection(_)
            | Pdu::GetPaneDirection(_)
            | Pdu::AdjustPaneSize(_)
            | Pdu::SpawnPaneInStack(_)
            | Pdu::ActivatePaneInStack(_)
            | Pdu::MovePaneToStack(_)
            | Pdu::GetThinkTermTree(_)
            | Pdu::MutateThinkTermTree(_)
            | Pdu::GetThinkTermSessionState(_)
            | Pdu::EnsureThinkTermThread(_)
            | Pdu::SetClientViewport(_)
            | Pdu::ClaimClientViewport(_)
            | Pdu::SetClientView(_)
            | Pdu::SetFrontendAccessMode(_)
            | Pdu::GetAgentStatuses(_)
            | Pdu::PluginFrame(_)
    )
}

pub struct SessionHandler {
    to_write_tx: PduSender,
    peer: ConnectionPeer,
    per_pane: HashMap<TabId, Arc<Mutex<PerPane>>>,
    client_id: Option<Arc<ClientId>>,
    client_registration: Option<ClientRegistrationId>,
    palette_session_id: Option<PaletteSessionId>,
    proxy_client_id: Option<ClientId>,
    /// This client's connection to the plugin host, from its first
    /// `PluginFrame` on; it closes when this does, or when the client
    /// sends an empty frame.
    plugin_pipe: Option<crate::plugin_relay::Pipe>,
}

impl Drop for SessionHandler {
    fn drop(&mut self) {
        if let Some(session_id) = self.palette_session_id.take() {
            schedule_palette_session_cleanup(session_id, "client disconnect");
        }
        if let (Some(client_id), Some(registration)) =
            (self.client_id.take(), self.client_registration.take())
        {
            Mux::get().unregister_client(&client_id, registration);
        }
    }
}

impl SessionHandler {
    pub fn new(to_write_tx: PduSender) -> Self {
        Self::for_peer(to_write_tx, ConnectionPeer::Local)
    }

    pub fn for_peer(to_write_tx: PduSender, peer: ConnectionPeer) -> Self {
        Self {
            to_write_tx,
            peer,
            per_pane: HashMap::new(),
            client_id: None,
            client_registration: None,
            palette_session_id: None,
            proxy_client_id: None,
            plugin_pipe: None,
        }
    }

    /// Tell a client that has just registered where the frontend lease
    /// stands: the access mode and every tab's ownership.
    ///
    /// The server otherwise pushes this state only when it changes, and a
    /// renderer will not publish its own viewport until it knows whether
    /// the tab is spoken for. A renderer reattaching to a tab that it, or
    /// its predecessor, used to drive therefore waited for a push that was
    /// never going to come, and sat behind "Restoring terminal state".
    /// Queued behind the registration's acknowledgement on the same
    /// channel, so it arrives after it.
    /// The palette the server's own configuration resolves to. Browsers use
    /// it as their base palette; a GUI client renders from its own config and
    /// ignores the push.
    fn push_default_palette(&self) {
        let _ = self.to_write_tx.send(DecodedPdu {
            serial: 0,
            pdu: Pdu::DefaultPalette(codec::DefaultPalette {
                palette: configured_default_palette(),
            }),
        });
    }

    fn push_frontend_state(&self) {
        let mux = Mux::get();
        let mut pdus = vec![Pdu::FrontendAccessState(codec_access_state(
            mux.frontend_access_state(),
        ))];
        let tab_ids: Vec<TabId> = mux
            .iter_windows()
            .into_iter()
            .filter_map(|window_id| mux.get_window(window_id))
            .flat_map(|window| window.iter().map(|tab| tab.tab_id()).collect::<Vec<_>>())
            .collect();
        for tab_id in tab_ids {
            if let Some(state) = mux.frontend_viewport_state(tab_id) {
                pdus.push(Pdu::ClientViewportState(codec_viewport_state(state)));
            }
        }
        for pdu in pdus {
            if self
                .to_write_tx
                .send(DecodedPdu { serial: 0, pdu })
                .is_err()
            {
                return;
            }
        }
    }

    /// The pane is gone: drop what was kept for it, the sent-image cache
    /// above all, which otherwise outlived every pane this connection ever
    /// saw.
    pub(crate) fn forget_pane(&mut self, pane_id: PaneId) {
        self.per_pane.remove(&pane_id);
    }

    pub(crate) fn per_pane(&mut self, pane_id: PaneId) -> Arc<Mutex<PerPane>> {
        Arc::clone(
            self.per_pane
                .entry(pane_id)
                .or_insert_with(|| Arc::new(Mutex::new(PerPane::default()))),
        )
    }

    /// Queue terminal input for `pane_id` and see that a drain is running.
    fn queue_pane_input(
        &mut self,
        pane_id: PaneId,
        input: PaneInput,
        respond: impl FnOnce(anyhow::Result<Pdu>) + Send + 'static,
    ) {
        let per_pane = self.per_pane(pane_id);
        let queued = per_pane.lock().unwrap().queue_input(QueuedInput {
            input,
            respond: Box::new(respond),
        });
        let start = match queued {
            Ok(start) => start,
            Err(refused) => {
                log::warn!(
                    "refusing {:?} for pane {pane_id}: {} bytes of input already wait for it",
                    refused,
                    INPUT_QUEUE_LIMIT
                );
                (refused.respond)(Err(anyhow!(
                    "too much input is already waiting for pane {pane_id}"
                )));
                return;
            }
        };
        if start {
            let source = InputSource {
                client_id: self.client_id.clone(),
                registration: self.client_registration,
                palette_session_id: self.palette_session_id,
            };
            spawn_into_main_thread(drain_pane_inputs(
                pane_id,
                self.to_write_tx.clone(),
                per_pane,
                source,
            ))
            .detach();
        }
    }

    pub fn schedule_pane_push(&mut self, pane_id: PaneId) {
        let sender = self.to_write_tx.clone();
        let per_pane = self.per_pane(pane_id);
        // One push on its way per pane. A burst of output notifications
        // used to queue a task each, and all but the first found nothing
        // new to send.
        if !per_pane.lock().unwrap().claim_push() {
            return;
        }
        spawn_into_main_thread(push_pane_changes_when_free(pane_id, sender, per_pane)).detach();
    }

    pub fn process_one(&mut self, decoded: DecodedPdu) {
        let start = Instant::now();
        let sender = self.to_write_tx.clone();
        let serial = decoded.serial;
        let palette_session_id = self.palette_session_id;
        log::trace!("recv {} {}", serial, decoded.pdu.pdu_name());

        if let (Some(client_id), Some(registration)) = (&self.client_id, self.client_registration) {
            if decoded.pdu.is_user_input() {
                let mux = Mux::get();
                mux.registered_client_had_input(client_id, registration);
            }
        }

        let send_response = move |result: anyhow::Result<Pdu>| {
            let pdu = match result {
                Ok(pdu) => pdu,
                Err(err) => Pdu::ErrorResponse(ErrorResponse {
                    reason: format!("Error: {err:#}"),
                }),
            };
            log::trace!("{} processing time {:?}", serial, start.elapsed());
            sender.send(DecodedPdu { pdu, serial }).ok();
        };

        // A web session is admitted by a revocable, expiring token; it
        // may do everything the user could at a keyboard, and nothing that
        // would turn the token into another credential. The list names
        // what is allowed, so a request added later is refused here until
        // someone decides it belongs.
        if matches!(self.peer, ConnectionPeer::Web(_)) && !web_peer_may_send(&decoded.pdu) {
            send_response(Err(anyhow!(
                "{} is not available to a web session",
                decoded.pdu.pdu_name()
            )));
            return;
        }

        if requires_existing_frontend_access(&decoded.pdu) {
            if let Err(err) = require_registered_frontend_access(
                &Mux::get(),
                self.client_id.as_ref(),
                self.client_registration,
            ) {
                send_response(Err(err));
                return;
            }
        }

        fn catch<F, SND>(f: F, send_response: SND)
        where
            F: FnOnce() -> anyhow::Result<Pdu>,
            SND: Fn(anyhow::Result<Pdu>),
        {
            send_response(f());
        }

        match decoded.pdu {
            Pdu::Ping(Ping {}) => send_response(Ok(Pdu::Pong(Pong {}))),
            Pdu::SetWindowWorkspace(SetWindowWorkspace {
                window_id,
                workspace,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut window = mux
                                .get_window_mut(window_id)
                                .ok_or_else(|| anyhow!("window {} is invalid", window_id))?;
                            window.set_workspace(&workspace);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::SetClientId(SetClientId {
                mut client_id,
                is_proxy,
            }) => {
                // Acknowledged first. Registering below can move the lease,
                // which publishes from the main thread; queued now, the
                // acknowledgement is on the wire ahead of that. Nothing is
                // lost by the order: the next request on this connection
                // is read only after this arm has returned.
                send_response(Ok(Pdu::UnitResponse(UnitResponse {})));
                if is_proxy {
                    if self.proxy_client_id.is_none() {
                        // Copy proxy identity, but don't assign it to the mux;
                        // we'll use it to annotate the actual clients own
                        // identity when they send it
                        self.proxy_client_id.replace(client_id);
                    }
                } else {
                    // If this session is a proxy, override the incoming id with
                    // the proxy information so that it is clear what is going
                    // on from the `thinkterm cli list-clients` information
                    if let Some(proxy_id) = &self.proxy_client_id {
                        client_id.ssh_auth_sock = proxy_id.ssh_auth_sock.clone();
                        // Note that this `via proxy pid` string is coupled
                        // with the logic in mux/src/ssh_agent
                        client_id.hostname =
                            format!("{} (via proxy pid {})", client_id.hostname, proxy_id.pid);
                    }
                    if let ConnectionPeer::Web(web) = &self.peer {
                        client_id.username = web.username.clone();
                        client_id.hostname = format!("web:{}", web.label);
                        client_id.ssh_auth_sock = None;
                    }

                    let client_id = Arc::new(client_id);
                    if let Some(old_session_id) = self.palette_session_id.take() {
                        schedule_palette_session_cleanup(old_session_id, "client identity change");
                    }
                    self.palette_session_id =
                        Some(Mux::get().register_palette_session(client_id.as_ref()));
                    if let (Some(old_client), Some(old_registration)) =
                        (self.client_id.take(), self.client_registration.take())
                    {
                        Mux::get().unregister_client(&old_client, old_registration);
                    }
                    let registration = Mux::get().register_client(client_id.clone());
                    self.client_id.replace(client_id);
                    self.client_registration.replace(registration);
                }
                if !is_proxy {
                    self.push_frontend_state();
                    self.push_default_palette();
                }
            }
            Pdu::SetFocusedPane(SetFocusedPane {
                pane_id,
                configured_palette,
            }) => {
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let _identity = mux.with_identity(client_id.clone());

                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;
                            require_registered_frontend_access(
                                &mux,
                                client_id.as_ref(),
                                registration,
                            )?;

                            if let Some(palette) = configured_palette {
                                if let Some(palette) = mux.advise_client_palette(
                                    palette_session_id.ok_or_else(|| {
                                        anyhow!(
                                            "palette-bearing focus requires a live client session"
                                        )
                                    })?,
                                    pane_id,
                                    palette,
                                ) {
                                    apply_client_palette(&pane, Some(palette))?;
                                }
                            }
                            // Switch the OSC query base before focus reporting
                            // can provoke an immediate query from the app.
                            activate_client_palette(&mux, &pane, palette_session_id)?;

                            let (_domain_id, window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;
                            {
                                let mut window =
                                    mux.get_window_mut(window_id).ok_or_else(|| {
                                        anyhow::anyhow!("window {window_id} not found")
                                    })?;
                                let tab_idx = window.idx_by_id(tab_id).ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "tab {tab_id} isn't really in window {window_id}!?"
                                    )
                                })?;
                                window.save_and_then_set_active(tab_idx);
                            }
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow::anyhow!("tab {tab_id} not found"))?;
                            tab.set_active_pane(&pane);

                            mux.record_focus_for_current_identity(pane_id);
                            mux.notify(mux::MuxNotification::PaneFocused(pane_id));

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::GetAgentStatuses(GetAgentStatuses {}) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            // The flat pane list first: this query runs on
                            // every client resync, and most rounds find no
                            // agents at all -- the window/tab walk below
                            // exists only to name their workspaces, so it
                            // must not run before we know any are needed.
                            // iter_panes + agent_status() rather than the
                            // detector registry: a chained mux correctly
                            // reports what it mirrors as well as what it
                            // detected itself.
                            let mut agents = vec![];
                            for pane in mux.iter_panes() {
                                let Some(status) = pane.agent_status() else {
                                    continue;
                                };
                                agents.push((pane.pane_id(), status, pane.get_title()));
                            }
                            if agents.is_empty() {
                                return Ok(Pdu::GetAgentStatusesResponse(
                                    GetAgentStatusesResponse { statuses: vec![] },
                                ));
                            }
                            // One pass over the window/tab topology:
                            // resolve_pane_id per agent pane would rescan
                            // every tab per entry.
                            let mut workspace_by_pane = std::collections::HashMap::new();
                            for window_id in mux.iter_windows() {
                                let Some(window) = mux.get_window(window_id) else {
                                    continue;
                                };
                                let workspace = window.get_workspace().to_string();
                                for tab in window.iter() {
                                    for pane in tab.iter_all_panes() {
                                        workspace_by_pane.insert(pane.pane_id(), workspace.clone());
                                    }
                                }
                            }
                            let statuses = agents
                                .into_iter()
                                .map(|(pane_id, status, title)| AgentStatusEntry {
                                    pane_id,
                                    status,
                                    title,
                                    workspace: workspace_by_pane
                                        .get(&pane_id)
                                        .cloned()
                                        .unwrap_or_default(),
                                })
                                .collect();
                            Ok(Pdu::GetAgentStatusesResponse(GetAgentStatusesResponse {
                                statuses,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::GetClientList(GetClientList) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let clients = mux.iter_clients();
                            Ok(Pdu::GetClientListResponse(GetClientListResponse {
                                clients,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::ListPanes(ListPanes {}) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut tabs = vec![];
                            let mut tab_titles = vec![];
                            let mut window_titles = HashMap::new();
                            for window_id in mux.iter_windows().into_iter() {
                                let window = mux.get_window(window_id).unwrap();
                                window_titles.insert(window_id, window.get_title().to_string());
                                for tab in window.iter() {
                                    tabs.push(tab.codec_pane_tree());
                                    tab_titles.push(tab.get_title());
                                }
                            }
                            log::trace!("ListPanes {tabs:#?} {tab_titles:?}");
                            Ok(Pdu::ListPanesResponse(ListPanesResponse {
                                tabs,
                                tab_titles,
                                window_titles,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::RenameWorkspace(RenameWorkspace {
                old_workspace,
                new_workspace,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            mux.rename_workspace(&old_workspace, &new_workspace);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }

            Pdu::WriteToPane(WriteToPane { pane_id, data }) => {
                self.queue_pane_input(pane_id, PaneInput::Write(data), send_response);
            }
            Pdu::EraseScrollbackRequest(EraseScrollbackRequest {
                pane_id,
                erase_mode,
            }) => {
                self.queue_pane_input(
                    pane_id,
                    PaneInput::EraseScrollback(erase_mode),
                    send_response,
                );
            }
            Pdu::KillPane(KillPane { pane_id }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            pane.kill();
                            // No push for a pane that is gone: PaneRemoved
                            // tells the client everything it needs.
                            mux.remove_pane(pane_id);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                })
                .detach();
            }
            Pdu::SendPaste(SendPaste { pane_id, data }) => {
                self.queue_pane_input(pane_id, PaneInput::Paste(data), send_response);
            }

            Pdu::SearchScrollbackRequest(SearchScrollbackRequest {
                pane_id,
                pattern,
                range,
                limit,
            }) => {
                use mux::pane::Pattern;

                async fn do_search(
                    pane_id: TabId,
                    pattern: Pattern,
                    range: std::ops::Range<StableRowIndex>,
                    limit: Option<u32>,
                    sender: PduSender,
                ) -> anyhow::Result<Pdu> {
                    // The search holds the terminal for its whole run, on
                    // the main thread: only once the pane is free.
                    let Some(pane) = pane_when_free(
                        pane_id,
                        &sender,
                        "a scrollback search",
                        Some(PUSH_DEFERRAL_LIMIT),
                    )
                    .await?
                    else {
                        anyhow::bail!("pane {pane_id} is busy");
                    };

                    pane.search(pattern, range, limit).await.map(|results| {
                        Pdu::SearchScrollbackResponse(SearchScrollbackResponse { results })
                    })
                }

                let sender = self.to_write_tx.clone();
                spawn_into_main_thread(async move {
                    promise::spawn::spawn(async move {
                        let result = do_search(pane_id, pattern, range, limit, sender).await;
                        send_response(result);
                    })
                    .detach();
                })
                .detach();
            }

            Pdu::SetPaneZoomed(SetPaneZoomed {
                containing_tab_id,
                pane_id,
                zoomed,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(containing_tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", containing_tab_id))?;
                            activate_client_palette(&mux, &pane, palette_session_id)?;
                            mux::zoom_trace!(
                                "srv.zoom.recv tab={containing_tab_id} pane={pane_id} \
                                 want_zoomed={zoomed} | {}",
                                tab.geometry_trace()
                            );
                            match tab.get_zoomed_pane() {
                                Some(p) => {
                                    let is_zoomed = p.pane_id() == pane_id;
                                    if is_zoomed != zoomed {
                                        tab.set_zoomed(false);
                                        if zoomed {
                                            tab.set_active_pane(&pane);
                                            tab.set_zoomed(zoomed);
                                        }
                                    }
                                }
                                None => {
                                    if zoomed {
                                        tab.set_active_pane(&pane);
                                        tab.set_zoomed(zoomed);
                                    }
                                }
                            }
                            mux::zoom_trace!(
                                "srv.zoom.done tab={containing_tab_id} pane={pane_id} \
                                 want_zoomed={zoomed} | {}",
                                tab.geometry_trace()
                            );
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::GetPaneDirection(GetPaneDirection { pane_id, direction }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let (_domain_id, _window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            let panes = tab.iter_panes_ignoring_zoom();
                            let pane_id = tab
                                .get_pane_direction(direction, true)
                                .map(|pane_index| panes[pane_index].pane.pane_id());

                            Ok(Pdu::GetPaneDirectionResponse(GetPaneDirectionResponse {
                                pane_id,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::ActivatePaneDirection(ActivatePaneDirection { pane_id, direction }) => {
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            claim_viewport_for_pane(
                                &mux,
                                client_id.as_ref(),
                                registration,
                                pane_id,
                            )?;
                            let (_domain_id, _window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            // A direction request is a no-op while zoomed when
                            // the configured policy forbids unzooming. Do not
                            // transfer ownership to a pane that won't receive
                            // focus in that case.
                            if tab.get_zoomed_pane().is_none()
                                || config::configuration().unzoom_on_switch_pane
                            {
                                let panes = tab.iter_panes_ignoring_zoom();
                                if let Some(pane_index) = tab.get_pane_direction(direction, true) {
                                    let target = &panes[pane_index].pane;
                                    activate_client_palette(&mux, target, palette_session_id)?;
                                }
                            }
                            tab.activate_pane_direction(direction);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::Resize(Resize {
                containing_tab_id,
                pane_id,
                size,
            }) => {
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            if let (Some(client_id), Some(registration)) =
                                (client_id.as_ref(), registration)
                            {
                                match mux.registered_client_may_resize_tab(
                                    client_id,
                                    registration,
                                    containing_tab_id,
                                ) {
                                    None => anyhow::bail!("client connection was superseded"),
                                    // An error, not a silent success: the
                                    // client reflowed itself before asking,
                                    // and "ok" would leave it showing a grid
                                    // the server does not have, for ever.
                                    Some(false) => anyhow::bail!(
                                        "resize refused: this client does not own the viewport of tab {containing_tab_id}"
                                    ),
                                    Some(true) => {}
                                }
                            }
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let tab = mux
                                .get_tab(containing_tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", containing_tab_id))?;
                            if !tab.contains_pane(pane_id) {
                                anyhow::bail!("pane {pane_id} is not in tab {containing_tab_id}");
                            }
                            pane.resize(size)?;
                            // A legacy single-pane Resize carries no pane
                            // frame. It may update the PTY surface, but it
                            // cannot safely redefine split geometry: font
                            // scaling and frontend chrome make the surface
                            // smaller than its containing rectangle. Complete
                            // Native viewports carry exact frames and are the
                            // sole source for divider reconstruction.
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::SendKeyDown(SendKeyDown {
                pane_id,
                event,
                input_serial,
            }) => {
                self.queue_pane_input(
                    pane_id,
                    PaneInput::Key {
                        event,
                        input_serial,
                    },
                    send_response,
                );
            }
            Pdu::SendMouseEvent(SendMouseEvent { pane_id, event }) => {
                self.queue_pane_input(pane_id, PaneInput::Mouse(event), send_response);
            }

            Pdu::SpawnV2(spawn) => {
                spawn_into_main_thread(async move {
                    schedule_domain_spawn_v2(spawn, send_response);
                })
                .detach();
            }

            Pdu::SplitPane(split) => {
                spawn_into_main_thread(async move {
                    schedule_split_pane(split, send_response);
                })
                .detach();
            }

            Pdu::SpawnPaneInStack(request) => {
                spawn_into_main_thread(async move {
                    schedule_spawn_pane_in_stack(request, send_response);
                })
                .detach();
            }

            Pdu::ActivatePaneInStack(request) => {
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    let mux = Mux::get();
                    let _identity = mux.with_identity(client_id.clone());
                    let result = (|| {
                        let pane = mux
                            .get_pane(request.pane_id)
                            .ok_or_else(|| anyhow!("no such pane {}", request.pane_id))?;
                        claim_viewport_for_pane(
                            &mux,
                            client_id.as_ref(),
                            registration,
                            request.pane_id,
                        )?;
                        let (_domain_id, _window_id, tab_id) = mux
                            .resolve_pane_id(request.pane_id)
                            .ok_or_else(|| anyhow!("no such pane {}", request.pane_id))?;
                        let tab = mux
                            .get_tab(tab_id)
                            .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                        if let Some(zoomed) = tab.get_zoomed_pane() {
                            let zoomed_stack = tab.pane_stack_id(zoomed.pane_id());
                            let target_stack = tab.pane_stack_id(request.pane_id);
                            if zoomed_stack.is_none() || zoomed_stack != target_stack {
                                anyhow::bail!("cannot switch pane tab while zoomed");
                            }
                        }
                        activate_client_palette(&mux, &pane, palette_session_id)?;
                        mux.activate_pane_in_stack(request.pane_id)
                    })();
                    send_response(result.map(|()| Pdu::UnitResponse(UnitResponse {})));
                })
                .detach();
            }

            Pdu::MovePaneToStack(request) => {
                spawn_into_main_thread(async move {
                    schedule_move_pane_to_stack(request, send_response);
                })
                .detach();
            }

            Pdu::GetThinkTermTree(_) => {
                send_response(Ok(Pdu::ThinkTermTreeState(ThinkTermTreeState {
                    tree: crate::thinkterm_tree::snapshot(),
                })));
            }

            Pdu::GetThinkTermSessionState(_) => {
                // The snapshot asks every pane for its progress and title
                // under the mux's window lock; done on the main thread so a
                // busy pane holds up this request, not this thread's other
                // connections and, through that lock, the mux itself.
                spawn_into_main_thread(async move {
                    send_response(
                        crate::thinkterm_session::snapshot().map(Pdu::ThinkTermSessionState),
                    );
                })
                .detach();
            }

            Pdu::EnsureThinkTermThread(request) => {
                spawn_into_main_thread(async move {
                    schedule_ensure_thinkterm_thread(request, send_response);
                })
                .detach();
            }

            Pdu::SetClientViewport(SetClientViewport { tab_id, viewport }) => {
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let client_id = client_id.ok_or_else(|| {
                                anyhow!("SetClientViewport requires an identified client")
                            })?;
                            let registration = registration.ok_or_else(|| {
                                anyhow!("SetClientViewport requires a live client registration")
                            })?;
                            let mux = Mux::get();
                            let state = mux
                                .set_registered_client_viewport(
                                    &client_id,
                                    registration,
                                    tab_id,
                                    mux_viewport(viewport),
                                )?
                                .ok_or_else(|| anyhow!("client connection was superseded"))?;
                            Ok(Pdu::ClientViewportState(codec_viewport_state(state)))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            // Offering a view is not a claim: a renderer that is not driving is
            // ignored, so this can be sent freely without stealing the lease.
            Pdu::SetClientView(codec::SetClientView { tab_id, view }) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let client_id = client_id.ok_or_else(|| {
                                anyhow!("SetClientView requires an identified client")
                            })?;
                            Mux::get().set_client_view(&client_id, tab_id, mux_view(view));
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::ClaimClientViewport(ClaimClientViewport { tab_id, viewport }) => {
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let client_id = client_id.ok_or_else(|| {
                                anyhow!("ClaimClientViewport requires an identified client")
                            })?;
                            let registration = registration.ok_or_else(|| {
                                anyhow!("ClaimClientViewport requires a live client registration")
                            })?;
                            let state = Mux::get()
                                .claim_registered_client_viewport(
                                    &client_id,
                                    registration,
                                    tab_id,
                                    mux_viewport(viewport),
                                )?
                                .ok_or_else(|| anyhow!("client connection was superseded"))?;
                            Ok(Pdu::ClientViewportState(codec_viewport_state(state)))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::SetFrontendAccessMode(SetFrontendAccessMode {
                mode,
                tab_id,
                viewport,
            }) => {
                let client_id = self.client_id.clone();
                let registration = self.client_registration;
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let client_id = client_id.ok_or_else(|| {
                                anyhow!("SetFrontendAccessMode requires an identified client")
                            })?;
                            let registration = registration.ok_or_else(|| {
                                anyhow!("SetFrontendAccessMode requires a live client registration")
                            })?;
                            let mux = Mux::get();
                            let target_mode = mux_access_mode(mode);
                            let viewport = mux_viewport(viewport);
                            mux.validate_registered_frontend_access_mode_change(
                                &client_id,
                                registration,
                                target_mode,
                                tab_id,
                                &viewport,
                            )?;
                            let prior_mode = mux.frontend_access_state().mode;
                            crate::thinkterm_access::persist_mode(target_mode)?;
                            match mux.set_registered_frontend_access_mode(
                                &client_id,
                                registration,
                                target_mode,
                                tab_id,
                                viewport,
                            ) {
                                Ok(state) => {
                                    Ok(Pdu::FrontendAccessState(codec_access_state(state)))
                                }
                                Err(err) => {
                                    if prior_mode != target_mode {
                                        if let Err(rollback) =
                                            crate::thinkterm_access::persist_mode(prior_mode)
                                        {
                                            log::error!(
                                                "failed to roll back persisted frontend mode: \
                                                 {rollback:#}"
                                            );
                                        }
                                    }
                                    Err(err)
                                }
                            }
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::MutateThinkTermTree(MutateThinkTermTree { ops }) => {
                // mutate() broadcasts to every connection when the batch
                // changed something; the direct response here is what lets the
                // caller reconcile even when it did not. On the main thread:
                // it walks the mux and persists to disk, and its broadcast
                // must follow this response.
                spawn_into_main_thread(async move {
                    send_response(
                        crate::thinkterm_tree::mutate(&ops)
                            .map(|tree| Pdu::ThinkTermTreeState(ThinkTermTreeState { tree })),
                    );
                })
                .detach();
            }

            Pdu::MoveTab(MoveTab { window_id, tab_id, index }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut window = mux
                                .get_window_mut(window_id)
                                .ok_or_else(|| anyhow!("no such window {window_id}"))?;
                            let from = window
                                .idx_by_id(tab_id)
                                .ok_or_else(|| anyhow!("tab {tab_id} is not in window {window_id}"))?;
                            let index = index.min(window.len().saturating_sub(1));
                            let active = window.get_active().map(|tab| tab.tab_id());
                            if from != index {
                                let tab = window.remove_by_idx(from);
                                window.insert(index, &tab);
                                if let Some(active) = active.and_then(|id| window.idx_by_id(id)) {
                                    window.set_active_without_saving(active);
                                }
                            }
                            drop(window);
                            // A reorder is a topology change to the mirrors:
                            // `TabAddedToWindow` is what reaches them and
                            // has them list the window again.
                            mux.notify(mux::MuxNotification::TabAddedToWindow { tab_id, window_id });
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::MovePaneToNewTab(request) => {
                let client_id = self.client_id.clone();
                spawn_into_main_thread(async move {
                    schedule_move_pane(request, send_response, client_id);
                })
                .detach();
            }

            Pdu::GetPaneRenderableDimensions(GetPaneRenderableDimensions { pane_id }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            // Without waiting for the pane's parser: this
                            // runs on the thread that serves every pane.
                            let summary = pane.summary_without_waiting();
                            Ok(Pdu::GetPaneRenderableDimensionsResponse(
                                GetPaneRenderableDimensionsResponse {
                                    pane_id,
                                    cursor_position: summary.cursor_position,
                                    dimensions: summary.dimensions,
                                },
                            ))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::GetPaneRenderChanges(GetPaneRenderChanges { pane_id, .. }) => {
                let sender = self.to_write_tx.clone();
                let per_pane = self.per_pane(pane_id);
                spawn_into_main_thread(async move {
                    let is_alive = Mux::get().get_pane(pane_id).is_some();
                    // The client's fallback poll for a pane it has heard
                    // nothing from -- which is just the pane whose parser
                    // may be holding its terminal lock. The pane is read
                    // the way a push reads it, once it can be read without
                    // waiting, rather than on this thread now; the answer
                    // does not wait for that, since the push carries the
                    // changes and the poll only asks whether the pane
                    // still exists.
                    if is_alive && per_pane.lock().unwrap().claim_push() {
                        spawn_into_main_thread(push_pane_changes_when_free(
                            pane_id, sender, per_pane,
                        ))
                        .detach();
                    }
                    send_response(Ok(Pdu::LivenessResponse(LivenessResponse {
                        pane_id,
                        is_alive,
                    })));
                })
                .detach();
            }

            Pdu::GetLines(GetLines { pane_id, lines }) => {
                let per_pane = self.per_pane(pane_id);
                let sender = self.to_write_tx.clone();
                spawn_into_main_thread(async move {
                    // Read once the pane can be read without waiting, like
                    // a push: the client asks again for rows it does not
                    // get, and the main thread has everyone else's work.
                    let pane = match pane_when_free(
                        pane_id,
                        &sender,
                        "a row fetch",
                        Some(PUSH_DEFERRAL_LIMIT),
                    )
                    .await
                    {
                        Ok(Some(pane)) => pane,
                        Ok(None) => {
                            send_response(Err(anyhow!("pane {pane_id} is busy")));
                            return;
                        }
                        Err(err) => {
                            send_response(Err(err));
                            return;
                        }
                    };
                    catch(
                        move || {
                            let mut lines_and_indices = vec![];

                            for range in lines {
                                let (first_row, lines) = pane.get_lines(range);
                                for (idx, mut line) in lines.into_iter().enumerate() {
                                    let stable_row = first_row + idx as StableRowIndex;
                                    line.compress_for_scrollback();
                                    lines_and_indices.push((stable_row, line));
                                }
                            }
                            per_pane
                                .lock()
                                .unwrap()
                                .sent_images
                                .remember(&lines_and_indices);
                            Ok(Pdu::GetLinesResponse(GetLinesResponse {
                                pane_id,
                                lines: lines_and_indices.into(),
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::GetImageCell(GetImageCell {
                pane_id,
                line_idx,
                cell_idx,
                data_hash,
                data_generation: _,
                have_frames,
            }) => {
                let per_pane = self.per_pane(pane_id);
                let sender = self.to_write_tx.clone();
                spawn_into_main_thread(async move {
                    // What was sent is answered from memory: the cell
                    // named here may have moved on since. Only a miss
                    // reads the pane, and then like a push: once it can
                    // be read without waiting.
                    let mut data = per_pane.lock().unwrap().sent_images.get(&data_hash);
                    let pane = if data.is_none() {
                        match pane_when_free(
                            pane_id,
                            &sender,
                            "an image fetch",
                            Some(PUSH_DEFERRAL_LIMIT),
                        )
                        .await
                        {
                            Ok(Some(pane)) => Some(pane),
                            Ok(None) => {
                                send_response(Err(anyhow!("pane {pane_id} is busy")));
                                return;
                            }
                            Err(err) => {
                                send_response(Err(err));
                                return;
                            }
                        }
                    } else {
                        None
                    };
                    catch(
                        move || {
                            if let Some(pane) = pane {
                                // The row asked about, if it is still in the
                                // buffer: a stable index that has scrolled
                                // away comes back as some other row (the
                                // oldest kept), whose picture would be filed
                                // under this hash and painted into this cell.
                                let (first, lines) = pane.get_lines(line_idx..line_idx + 1);
                                let lines = if first == line_idx { lines } else { vec![] };
                                // The picture now in the cell, should the one
                                // asked for be gone: a program streaming frames
                                // replaces it faster than a fetch can land, and
                                // for a stream the newest frame is the one
                                // wanted anyway. The client sees the hash it
                                // actually got.
                                let mut current = None;
                                'found_data: for line in lines {
                                    if let Some(cell) = line.get_cell(cell_idx) {
                                        if let Some(images) = cell.attrs().images() {
                                            for im in images {
                                                if im.image_data().hash() == data_hash {
                                                    data.replace(im.image_data().clone());
                                                    break 'found_data;
                                                }
                                                if current.is_none() {
                                                    current = Some(im.image_data().clone());
                                                }
                                            }
                                        }
                                    }
                                }
                                if data.is_none() {
                                    data = current;
                                }
                                if let Some(found) = &data {
                                    per_pane
                                        .lock()
                                        .unwrap()
                                        .sent_images
                                        .insert(Arc::clone(found));
                                }
                            }
                            let (data_generation, data, frames_from) = match &data {
                                Some(image) => {
                                    // The frames the client holds belong to
                                    // the image it asked for; a different
                                    // picture is sent whole, or its frames
                                    // would be cut at a count that means
                                    // nothing for it.
                                    let have_frames = if image.hash() == data_hash {
                                        have_frames
                                    } else {
                                        0
                                    };
                                    let (generation, payload, from) =
                                        crate::sent_images::reply_for(image, have_frames);
                                    (generation, Some(payload), from)
                                }
                                None => (0, None, 0),
                            };
                            Ok(Pdu::GetImageCellResponse(GetImageCellResponse {
                                pane_id,
                                data,
                                data_generation,
                                frames_from,
                            }))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::GetCodecVersion(_) => {
                match std::env::current_exe().context("resolving current_exe") {
                    Err(err) => send_response(Err(err)),
                    Ok(executable_path) => {
                        send_response(Ok(Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                            codec_vers: CODEC_VERSION,
                            version_string: config::wezterm_version().to_owned(),
                            server_id: Mux::get().runtime_server_id().to_string(),
                            executable_path,
                            config_file_path: std::env::var_os("WEZTERM_CONFIG_FILE")
                                .map(Into::into),
                        })))
                    }
                }
            }

            // Carried to the plugin host unread; see plugin_relay.
            Pdu::PluginFrame(PluginFrame { data }) => {
                if data.is_empty() {
                    self.plugin_pipe = None;
                    send_response(Ok(Pdu::UnitResponse(UnitResponse {})));
                    return;
                }
                if !self.plugin_pipe.as_ref().is_some_and(|pipe| pipe.is_open()) {
                    self.plugin_pipe =
                        Some(crate::plugin_relay::Pipe::open(self.to_write_tx.clone()));
                }
                if let Some(pipe) = &self.plugin_pipe {
                    pipe.send(data, Box::new(send_response));
                }
            }

            Pdu::GetServerOsRelease(_) => send_response(Ok(Pdu::GetServerOsReleaseResponse(
                GetServerOsReleaseResponse {
                    os_release_id: local_os_release_id().clone(),
                },
            ))),

            Pdu::WebTokenMint(WebTokenMint { label, ttl_secs }) => {
                catch(
                    move || {
                        let minted = crate::web_auth::WEB_TOKENS.mint(
                            label,
                            ttl_secs.map(Duration::from_secs),
                        )?;
                        let urls = web_urls(&crate::web_control::listening())
                            .into_iter()
                            // In the fragment: never sent to any server, never
                            // in a Referer, never in an access log.
                            .map(|url| format!("{url}#token={}", minted.token))
                            .collect();
                        Ok(Pdu::WebTokenMintResponse(WebTokenMintResponse {
                            id: minted.id,
                            label: minted.label,
                            token: minted.token,
                            expires_at: minted.expires_at,
                            certificates: web_certificates(),
                            urls,
                        }))
                    },
                    send_response,
                );
            }
            Pdu::WebTokenList(_) => {
                send_response(Ok(Pdu::WebTokenListResponse(WebTokenListResponse {
                    tokens: crate::web_auth::WEB_TOKENS.list(),
                })));
            }
            Pdu::WebTokenRevoke(WebTokenRevoke { id }) => {
                let revoked = crate::web_auth::WEB_TOKENS.revoke(id.as_deref());
                send_response(Ok(Pdu::WebTokenRevokeResponse(WebTokenRevokeResponse {
                    revoked,
                })));
            }
            Pdu::GetWebServerStatus(_) => {
                send_response(Ok(Pdu::WebServerStatus(web_server_status())));
            }
            Pdu::SetWebServer(SetWebServer {
                enabled,
                bind_address,
            }) => {
                // A client certificate says "this user", which is what lets
                // a remote peer do everything a local one can. It does not
                // say "on this machine", and opening a port the rest of the
                // network can reach is not a thing to be able to arrange
                // from off the machine: the port outlives the connection
                // that asked for it, and a token minted through it outlives
                // the certificate, which cannot be revoked. The reverse
                // direction was already closed -- see `web_peer_may_send`,
                // which keeps a browser from turning its token into a
                // certificate -- and this is the same argument.
                let on_this_machine = matches!(self.peer, ConnectionPeer::Local);
                catch(
                    move || {
                        let Some(control) = crate::web_control::get() else {
                            anyhow::bail!(
                                "this server has no web listener to turn on or off"
                            );
                        };
                        if enabled {
                            let server = web_server_to_start(bind_address.as_deref())?;
                            if !server.is_loopback() && !on_this_machine {
                                anyhow::bail!(
                                    "a client connected over TLS may not open a listener on {}; \
                                     turn it on from a session on that machine, or over ssh",
                                    server.bind_address
                                );
                            }
                            if !crate::web_control::listening().contains(&server.bind_address) {
                                // The token store is set up by whoever starts
                                // the first listener. A server configured with
                                // no `web_servers` never did it at startup, and
                                // an unconfigured store keeps tokens in memory
                                // and never sweeps the expired ones. A second
                                // call is a no-op; see `WebTokenStore::configure`.
                                (control.configure_tokens)(std::slice::from_ref(&server))?;
                                // The listener resumes admissions itself once
                                // the port is up; see `spawn_web_listener`.
                                (control.start)(&server)?;
                            }
                        } else {
                            // Every listener, not just the named one: a client
                            // that only knows "off" must not leave a second
                            // port open behind it.
                            for address in crate::web_control::listening() {
                                (control.stop)(&address);
                            }
                            // Closing the port only stops the next browser.
                            // Off has to mean the ones already inside, too.
                            let cut = crate::web_auth::WEB_TOKENS.disconnect_all();
                            if cut > 0 {
                                log::info!("web listener stopped; cut {cut} live browsers");
                            }
                        }
                        Ok(Pdu::WebServerStatus(web_server_status()))
                    },
                    send_response,
                );
            }
            Pdu::GetTlsCreds(_) => {
                catch(
                    move || {
                        let client_cert_pem = PKI.generate_client_cert()?;
                        let ca_cert_pem = PKI.ca_pem_string()?;
                        Ok(Pdu::GetTlsCredsResponse(GetTlsCredsResponse {
                            client_cert_pem,
                            ca_cert_pem,
                        }))
                    },
                    send_response,
                );
            }
            Pdu::WindowTitleChanged(WindowTitleChanged { window_id, title }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let mut window = mux
                                .get_window_mut(window_id)
                                .ok_or_else(|| anyhow!("no such window {window_id}"))?;

                            window.set_title(&title);

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::TabTitleChanged(TabTitleChanged { tab_id, title }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {tab_id}"))?;

                            tab.set_title(&title);

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }
            Pdu::SetPalette(SetPalette { pane_id, palette }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let pane = mux
                                .get_pane(pane_id)
                                .ok_or_else(|| anyhow!("no such pane {}", pane_id))?;
                            let palette_session_id = palette_session_id.ok_or_else(|| {
                                anyhow!("palette advisory requires a live client session")
                            })?;

                            // Advice from a background client is stored only.
                            // If this client already owns the pane, a config
                            // reload updates the OSC query base immediately.
                            if let Some(palette) =
                                mux.advise_client_palette(palette_session_id, pane_id, palette)
                            {
                                apply_client_palette(&pane, Some(palette))?;
                            }

                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::AdjustPaneSize(AdjustPaneSize {
                pane_id,
                direction,
                amount,
            }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get();
                            let (_pane_domain_id, _window_id, tab_id) = mux
                                .resolve_pane_id(pane_id)
                                .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;

                            let tab = match mux.get_tab(tab_id) {
                                Some(tab) => tab,
                                None => {
                                    return Err(anyhow!(
                                        "Failed to retrieve tab with ID {}",
                                        tab_id
                                    ));
                                }
                            };

                            tab.adjust_pane_size(direction, amount);
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                })
                .detach();
            }

            Pdu::Invalid { .. } => send_response(Err(anyhow!("invalid PDU {:?}", decoded.pdu))),
            // The answer to this connection's liveness probe. Arriving at
            // all is the answer; there is nothing to reply, and replying
            // with an error (as the catch-all below would) killed the
            // client that had just proved it was alive.
            Pdu::Pong(_) => {}
            Pdu::AgentStatusChanged { .. }
            | Pdu::GetAgentStatusesResponse { .. }
            | Pdu::ListPanesResponse { .. }
            | Pdu::SetApplicationPalette { .. }
            | Pdu::DefaultPalette { .. }
            | Pdu::SetClipboard { .. }
            | Pdu::NotifyAlert { .. }
            | Pdu::SpawnResponse { .. }
            | Pdu::GetPaneRenderChangesResponse { .. }
            | Pdu::GetServerOsReleaseResponse { .. }
            | Pdu::UnitResponse { .. }
            | Pdu::LivenessResponse { .. }
            | Pdu::GetPaneDirectionResponse { .. }
            | Pdu::SearchScrollbackResponse { .. }
            | Pdu::GetLinesResponse { .. }
            | Pdu::GetCodecVersionResponse { .. }
            | Pdu::WindowWorkspaceChanged { .. }
            | Pdu::GetTlsCredsResponse { .. }
            | Pdu::WebTokenMintResponse { .. }
            | Pdu::WebTokenListResponse { .. }
            | Pdu::WebTokenRevokeResponse { .. }
            | Pdu::WebServerStatus { .. }
            | Pdu::GetClientListResponse { .. }
            | Pdu::PaneRemoved { .. }
            | Pdu::PaneFocused { .. }
            | Pdu::TabResized { .. }
            | Pdu::GetImageCellResponse { .. }
            | Pdu::MovePaneToNewTabResponse { .. }
            | Pdu::TabAddedToWindow { .. }
            | Pdu::GetPaneRenderableDimensionsResponse { .. }
            | Pdu::ThinkTermTreeState { .. }
            | Pdu::ThinkTermSessionState { .. }
            | Pdu::EnsureThinkTermThreadResponse { .. }
            | Pdu::ClientViewportState { .. }
            | Pdu::FrontendAccessState { .. }
            | Pdu::ErrorResponse { .. } => {
                send_response(Err(anyhow!("expected a request, got {:?}", decoded.pdu)))
            }
        }
    }
}

// Dancing around a little bit here; we can't directly spawn_into_main_thread the domain_spawn
// function below because the compiler thinks that all of its locals then need to be Send.
// We need to shimmy through this helper to break that aspect of the compiler flow
// analysis and allow things to compile.
fn schedule_domain_spawn_v2<SND>(spawn: SpawnV2, send_response: SND)
where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(domain_spawn_v2(spawn).await) }).detach();
}

fn schedule_ensure_thinkterm_thread<SND>(request: EnsureThinkTermThread, send_response: SND)
where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(ensure_thinkterm_thread(request).await) })
        .detach();
}

async fn ensure_thinkterm_thread(request: EnsureThinkTermThread) -> anyhow::Result<Pdu> {
    let _materialize = THINKTERM_MATERIALIZE.lock().await;
    let mux = Mux::get();
    // No identity is installed here on purpose: every workspace name on this
    // path is explicit (`landing.workspace`), and a guard held across the
    // awaits below would leak the requesting identity to unrelated
    // main-thread work. See `Mux::with_identity`.
    let landing = crate::thinkterm_tree::ensure_landing(request.preferred_thread_id.as_deref())?;

    let has_live_pane = mux
        .iter_windows_in_workspace(&landing.workspace)
        .into_iter()
        .filter_map(|window_id| mux.get_window(window_id))
        .any(|window| window.iter().any(|tab| !tab.iter_all_panes().is_empty()));

    let spawned = if has_live_pane {
        false
    } else {
        crate::thinkterm_layout::begin_restore(&landing.workspace);
        let restored = crate::thinkterm_layout::restore_thread_layout(
            &landing.thread_id,
            &landing.workspace,
            &landing.project_path,
            request.size,
        )
        .await;

        let (materialized, preserve_saved_layout) = match restored {
            Ok(true) => (Ok(()), false),
            Ok(false) => (
                spawn_default_thinkterm_thread(&mux, &landing, request.size).await,
                false,
            ),
            Err(err) => {
                log::warn!(
                    "failed to restore ThinkTerm layout for thread {}: {err:#}; \
                     opening one default shell",
                    landing.thread_id
                );
                (
                    spawn_default_thinkterm_thread(&mux, &landing, request.size)
                        .await
                        .context("spawn fallback shell after layout restore failure"),
                    true,
                )
            }
        };

        match materialized {
            Ok(()) => {
                crate::thinkterm_layout::finish_restore(&landing.workspace, !preserve_saved_layout);
                crate::thinkterm_session::publish_changed();
                true
            }
            Err(err) => {
                crate::thinkterm_layout::finish_restore(&landing.workspace, false);
                return Err(err);
            }
        }
    };

    Ok(Pdu::EnsureThinkTermThreadResponse(
        EnsureThinkTermThreadResponse {
            thread_id: landing.thread_id,
            workspace: landing.workspace,
            spawned,
        },
    ))
}

/// NOTE: this runs with *no* identity installed (see `ensure_thinkterm_thread`).
/// If layout restore or this fallback ever gains a "send a command to the new
/// shell" step, that write would reach `record_input_for_current_identity`
/// and, in `TmuxLatest`, claim the tab's viewport for the ambient identity
/// (the GUI's, in a GUI-hosted mux) — resolve the requesting identity
/// explicitly at that point instead of re-adding a `with_identity` guard.
async fn spawn_default_thinkterm_thread(
    mux: &Mux,
    landing: &crate::thinkterm_tree::LandingRecord,
    size: wezterm_term::TerminalSize,
) -> anyhow::Result<()> {
    let command_dir = match landing.project_path.trim() {
        "" => None,
        path if path.starts_with("wezterm-mux://") => None,
        path => Some(path.to_string()),
    };
    mux.spawn_tab_or_window(
        None,
        SpawnTabDomain::DefaultDomain,
        None,
        command_dir,
        size,
        None,
        landing.workspace.clone(),
        None,
    )
    .await?;
    Ok(())
}

fn schedule_split_pane<SND>(split: SplitPane, send_response: SND)
where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(split_pane(split).await) }).detach();
}

fn schedule_spawn_pane_in_stack<SND>(request: SpawnPaneInStack, send_response: SND)
where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(spawn_pane_in_stack(request).await) })
        .detach();
}

fn schedule_move_pane_to_stack<SND>(request: MovePaneToStack, send_response: SND)
where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move {
        let mux = Mux::get();
        // No identity: `move_pane_to_stack` reads none, and a guard held
        // across the await would leak the identity to unrelated work.
        send_response(
            mux.move_pane_to_stack(request.source_pane_id, request.target_pane_id)
                .await
                .map(|_| Pdu::UnitResponse(UnitResponse {})),
        );
    })
    .detach();
}

async fn spawn_pane_in_stack(request: SpawnPaneInStack) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    // No identity: `spawn_pane_in_stack` reads none, and a guard held across
    // the await would leak the identity to unrelated main-thread work.

    let (_pane_domain_id, window_id, tab_id) = mux
        .resolve_pane_id(request.pane_id)
        .ok_or_else(|| anyhow!("pane_id {} invalid", request.pane_id))?;

    // The new pane joins the stack occupying the same rect as the base
    // pane, so it inherits the base pane's current size.
    let base = mux
        .get_pane(request.pane_id)
        .ok_or_else(|| anyhow!("pane_id {} invalid", request.pane_id))?;
    let dims = base.get_dimensions();
    let size = ::wezterm_term::TerminalSize {
        rows: dims.viewport_rows,
        cols: dims.cols,
        pixel_width: dims.pixel_width,
        pixel_height: dims.pixel_height,
        dpi: dims.dpi,
    };

    let pane = mux
        .spawn_pane_in_stack(
            request.pane_id,
            request.domain,
            request.command.map(|c| c.into_command_builder()),
            request.command_dir,
            size,
        )
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: pane.pane_id(),
        tab_id,
        window_id,
        size,
    }))
}

async fn split_pane(split: SplitPane) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    // No identity: `Mux::split_pane` reads none, and a guard held across the
    // await would leak the identity to unrelated main-thread work.

    let (_pane_domain_id, window_id, tab_id) = mux
        .resolve_pane_id(split.pane_id)
        .ok_or_else(|| anyhow!("pane_id {} invalid", split.pane_id))?;

    let source = if let Some(move_pane_id) = split.move_pane_id {
        SplitSource::MovePane(move_pane_id)
    } else {
        SplitSource::Spawn {
            command: split.command.map(|c| c.into_command_builder()),
            command_dir: split.command_dir,
        }
    };

    let (pane, size) = mux
        .split_pane(split.pane_id, split.split_request, source, split.domain)
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: pane.pane_id(),
        tab_id,
        window_id,
        size,
    }))
}

async fn domain_spawn_v2(spawn: SpawnV2) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    // No identity: `spawn.workspace` is an explicit non-optional string on
    // the wire, so `spawn_tab_or_window` never falls back to the ambient
    // identity's workspace. A guard held across the await would leak the
    // identity to unrelated main-thread work.

    let (tab, pane, window_id) = mux
        .spawn_tab_or_window(
            spawn.window_id,
            spawn.domain,
            spawn.command.map(|c| c.into_command_builder()),
            spawn.command_dir,
            spawn.size,
            None, // optional current pane_id
            spawn.workspace,
            None, // optional gui window position
        )
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        pane_id: pane.pane_id(),
        tab_id: tab.tab_id(),
        window_id,
        size: tab.get_size(),
    }))
}

fn schedule_move_pane<SND>(
    request: MovePaneToNewTab,
    send_response: SND,
    client_id: Option<Arc<ClientId>>,
) where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(move_pane(request, client_id).await) })
        .detach();
}

/// `Mux::move_pane_to_new_tab` falls back to the *global identity's*
/// workspace when it has to create a window and nobody named one. Resolve
/// that name here, synchronously and per-client, so no identity has to
/// survive the await.
///
/// Only the new-window case is filled in: with a `window_id` the workspace
/// is unused locally, and leaving it `None` keeps the PDU forwarded to a
/// nested mux byte-identical to before.
fn workspace_for_moved_pane(
    mux: &Mux,
    requested: Option<String>,
    window_id: Option<mux::window::WindowId>,
    client_id: Option<&Arc<ClientId>>,
) -> Option<String> {
    if requested.is_some() || window_id.is_some() {
        return requested;
    }
    Some(mux.active_workspace_for_optional_client(client_id))
}

async fn move_pane(
    request: MovePaneToNewTab,
    client_id: Option<Arc<ClientId>>,
) -> anyhow::Result<Pdu> {
    let mux = Mux::get();
    // No identity is installed here: the one identity-derived value on this
    // path (the fallback workspace for a new window) is resolved explicitly
    // below. A guard held across the await would leak the identity to
    // unrelated main-thread work.
    let workspace = workspace_for_moved_pane(
        &mux,
        request.workspace_for_new_window,
        request.window_id,
        client_id.as_ref(),
    );

    let (tab, window_id) = mux
        .move_pane_to_new_tab(request.pane_id, request.window_id, workspace)
        .await?;

    Ok::<Pdu, anyhow::Error>(Pdu::MovePaneToNewTabResponse(MovePaneToNewTabResponse {
        tab_id: tab.tab_id(),
        window_id,
    }))
}

#[cfg(test)]
mod web_server_address_tests {
    /// An address arriving over the wire is checked before it becomes a
    /// listener, so a typo names itself instead of surfacing as a bind
    /// error. A hostname is as valid as a literal -- `bind_address` takes
    /// either -- but the port is not optional.
    #[test]
    fn an_address_to_listen_on_needs_a_port_and_nothing_stranger() {
        for good in [
            "127.0.0.1:8088",
            "0.0.0.0:8088",
            "[::1]:8088",
            "[::]:8088",
            "mux.example.net:8443",
        ] {
            assert!(
                super::web_server_to_start(Some(good)).is_ok(),
                "{:?} should be a listenable address",
                good
            );
        }
        for bad in [
            "",
            "127.0.0.1",
            "mux.example.net",
            "127.0.0.1:",
            "127.0.0.1:notaport",
            "127.0.0.1:8088/ws",
            "http://127.0.0.1:8088",
            "127.0.0.1:8088 ",
        ] {
            let err = super::web_server_to_start(Some(bad)).unwrap_err().to_string();
            assert!(
                err.contains("address:port"),
                "{:?} was accepted, or refused for another reason: {}",
                bad,
                err
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        claim_viewport_for_pane, requires_existing_frontend_access, workspace_for_moved_pane,
        InputDrain, PaneInput, PerPane, QueuedInput,
    };
    use codec::{EnsureThinkTermThread, Pdu};
    use mux::client::ClientId;
    use mux::Mux;
    use std::sync::Arc;
    use wezterm_term::color::ColorPalette;
    use wezterm_term::TerminalSize;

    #[test]
    fn selecting_dirty_lines_preserves_content_and_outside_invalidation() {
        use rangeset::RangeSet;
        use termwiz::cell::CellAttributes;
        use wezterm_term::Line;

        for changed in [
            vec![],
            vec![101],
            vec![100, 102],
            vec![100, 101, 102, 103],
        ] {
            let mut dirty = RangeSet::new();
            dirty.add_range(10..20);
            dirty.add(104);
            for row in &changed {
                dirty.add(*row);
            }
            let lines = (100..104)
                .map(|row| {
                    Line::from_text(&format!("row {row}"), &CellAttributes::default(), 7, None)
                })
                .collect();
            // Use the actual first row returned by the pane, even if a
            // concurrent scroll moved it since the viewport was requested.
            let selected = super::select_dirty_lines(100, lines, &mut dirty);
            assert_eq!(
                selected.iter().map(|(row, _)| *row).collect::<Vec<_>>(),
                changed
            );
            for (row, line) in selected {
                assert_eq!(line.as_str(), format!("row {row}"));
                assert_eq!(line.current_seqno(), 7);
                assert!(line.is_compressed_for_scrollback());
            }
            assert_eq!(
                dirty.iter().cloned().collect::<Vec<_>>(),
                vec![10..20, 104..105]
            );
        }
    }

    #[test]
    fn an_empty_line_read_keeps_dirty_rows_for_recovery() {
        let mut dirty = rangeset::RangeSet::new();
        dirty.add_range(100..104);
        assert!(super::select_dirty_lines(100, vec![], &mut dirty).is_empty());
        assert_eq!(dirty.iter().cloned().collect::<Vec<_>>(), vec![100..104]);
    }

    #[test]
    fn cold_thread_materialization_does_not_require_an_existing_owner() {
        let request = Pdu::EnsureThinkTermThread(EnsureThinkTermThread {
            preferred_thread_id: Some("thread-main".to_string()),
            size: TerminalSize::default(),
        });

        assert!(!requires_existing_frontend_access(&request));
    }

    #[test]
    fn terminal_input_requires_identity_and_a_live_registration() {
        let mux = Mux::new(None);
        let missing_identity = claim_viewport_for_pane(&mux, None, None, usize::MAX)
            .unwrap_err()
            .to_string();
        assert!(missing_identity.contains("identified client"));

        let client = Arc::new(mux::client::generate_client_id());
        let missing_registration = claim_viewport_for_pane(&mux, Some(&client), None, usize::MAX)
            .unwrap_err()
            .to_string();
        assert!(missing_registration.contains("live client registration"));

        let stale_registration = mux.register_client(Arc::clone(&client));
        mux.unregister_client(&client, stale_registration);
        let superseded =
            claim_viewport_for_pane(&mux, Some(&client), Some(stale_registration), usize::MAX)
                .unwrap_err()
                .to_string();
        assert!(superseded.contains("superseded client connection"));
    }

    #[test]
    fn moved_pane_workspace_is_resolved_from_the_requesting_client() {
        config::use_test_configuration();
        let mux = Mux::new(None);
        let client = Arc::new(mux::client::generate_client_id());
        mux.register_client(Arc::clone(&client));
        mux.set_active_workspace_for_client(&client, "space-2");

        // An explicitly requested workspace passes through untouched.
        assert_eq!(
            workspace_for_moved_pane(&mux, Some("named".to_string()), None, Some(&client)),
            Some("named".to_string())
        );
        // An existing-window move keeps the field empty: it is unused locally
        // and the forwarded nested-mux PDU stays byte-identical.
        assert_eq!(
            workspace_for_moved_pane(&mux, None, Some(7), Some(&client)),
            None
        );
        // A new window with no name lands in the requesting client's
        // workspace...
        assert_eq!(
            workspace_for_moved_pane(&mux, None, None, Some(&client)),
            Some("space-2".to_string())
        );
        // ...and in the default workspace when the request is anonymous.
        assert_eq!(
            workspace_for_moved_pane(&mux, None, None, None),
            Some("default".to_string())
        );
    }

    #[test]
    fn a_registering_client_is_told_where_the_lease_stands() {
        use super::{PduSender, SessionHandler};
        use codec::{DecodedPdu, SetClientId};
        use std::sync::Mutex;

        // The registration path consults the process-wide mux, and parts of
        // it schedule follow-up work; a scheduler that merely queues it is
        // enough here.
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _executor = promise::spawn::SimpleExecutor::new();

        let sent: Arc<Mutex<Vec<DecodedPdu>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&sent);
        let mut handler = SessionHandler::new(PduSender::new(move |pdu| {
            sink.lock().unwrap().push(pdu);
            Ok(())
        }));
        handler.process_one(DecodedPdu {
            serial: 7,
            pdu: Pdu::SetClientId(SetClientId {
                client_id: mux::client::generate_client_id(),
                is_proxy: false,
            }),
        });

        let sent = sent.lock().unwrap();
        let names: Vec<(u64, &str)> = sent.iter().map(|d| (d.serial, d.pdu.pdu_name())).collect();
        assert_eq!(
            names,
            vec![
                (7, "UnitResponse"),
                (0, "FrontendAccessState"),
                (0, "DefaultPalette")
            ],
            "the acknowledgement first, then the state; no tabs, so no viewports"
        );
    }

    #[test]
    fn only_one_push_is_scheduled_per_pane_at_a_time() {
        let mut state = PerPane::default();
        assert!(state.claim_push(), "nothing on its way: schedule one");
        assert!(
            !state.claim_push(),
            "already on its way: it will carry this too"
        );
        state.release_push();
        assert!(
            state.claim_push(),
            "released: the next notification schedules again"
        );
    }

    #[test]
    fn application_palette_delivery_distinguishes_unsent_from_reset() {
        let mut state = PerPane::default();
        assert!(state.needs_application_palette(&None));

        state.record_application_palette(None);
        assert!(!state.needs_application_palette(&None));

        let palette = Some(ColorPalette::default());
        assert!(state.needs_application_palette(&palette));
        state.record_application_palette(palette.clone());
        assert!(!state.needs_application_palette(&palette));

        assert!(state.needs_application_palette(&None));
    }

    /// Input for a pane nobody is reading is not kept without limit.
    #[test]
    fn a_pane_with_too_much_input_waiting_refuses_more() {
        use super::{PaneInput, QueuedInput, INPUT_QUEUE_LIMIT};
        let mut per_pane = PerPane::default();
        let item = |bytes: usize| QueuedInput {
            input: PaneInput::Write(vec![b'x'; bytes]),
            respond: Box::new(|_| {}),
        };
        assert!(per_pane.queue_input(item(INPUT_QUEUE_LIMIT - 100)).is_ok());
        assert!(per_pane.queue_input(item(64)).is_ok(), "just fits");
        let refused = per_pane
            .queue_input(item(64))
            .expect_err("over the limit, handed back");
        assert!(matches!(refused.input, PaneInput::Write(_)));
        assert_eq!(per_pane.inputs.len(), 2);
        per_pane.pop_input();
        assert!(
            per_pane.queue_input(item(64)).is_ok(),
            "room again once the big one was applied"
        );
    }

    #[test]
    fn only_bytes_for_the_pty_skip_the_wait_for_a_free_pane() {
        assert!(!PaneInput::Write(vec![]).needs_the_terminal());
        assert!(PaneInput::Paste(String::new()).needs_the_terminal());
        assert!(PaneInput::EraseScrollback(
            config::keyassignment::ScrollbackEraseMode::ScrollbackOnly
        )
        .needs_the_terminal());
    }

    /// Input for a pane waits in its queue in the order it was sent; a
    /// drain that ends early answers what it leaves behind and lets the
    /// next input start a new one.
    #[test]
    fn queued_input_keeps_its_order_and_an_ended_drain_answers_what_is_left() {
        let per_pane = Arc::new(std::sync::Mutex::new(PerPane::default()));
        let answered: Arc<std::sync::Mutex<Vec<(&'static str, bool)>>> = Default::default();
        let item = |tag: &'static str| {
            let answered = Arc::clone(&answered);
            QueuedInput {
                input: PaneInput::Write(tag.as_bytes().to_vec()),
                respond: Box::new(move |result| {
                    answered.lock().unwrap().push((tag, result.is_ok()));
                }),
            }
        };
        assert!(
            per_pane.lock().unwrap().queue_input(item("a")).unwrap(),
            "the first input starts the drain"
        );
        assert!(
            !per_pane.lock().unwrap().queue_input(item("b")).unwrap(),
            "the second rides along"
        );

        // The drain takes the first piece and is then dropped, the way a
        // task is when the pane stays busy or the scheduler goes away.
        let first = per_pane.lock().unwrap().pop_input().unwrap();
        assert!(matches!(&first.input, PaneInput::Write(data) if data == b"a"));
        drop(InputDrain {
            per_pane: Arc::clone(&per_pane),
            why: "ended in the test",
            done: false,
        });
        assert_eq!(
            answered.lock().unwrap().as_slice(),
            &[("b", false)],
            "what was left is answered with an error, in order"
        );
        assert!(per_pane.lock().unwrap().inputs.is_empty());
        assert!(
            per_pane.lock().unwrap().queue_input(item("c")).unwrap(),
            "and the next input starts a new drain"
        );

        // A drain that found its queue empty is over: input queued after
        // that belongs to the drain it starts, and the old guard must not
        // take it.
        let mut drain = InputDrain {
            per_pane: Arc::clone(&per_pane),
            why: "ended in the test",
            done: false,
        };
        assert!(drain.next().is_some(), "c");
        assert!(drain.next().is_none(), "empty: the drain is over");
        assert!(
            per_pane.lock().unwrap().queue_input(item("d")).unwrap(),
            "so the next input starts a new one"
        );
        drop(drain);
        assert_eq!(
            per_pane.lock().unwrap().inputs.len(),
            1,
            "d is still queued for the new drain"
        );
        assert!(!answered.lock().unwrap().iter().any(|(tag, _)| *tag == "d"));
    }
}

/// What a client is told about the browser listener.
///
/// `urls` names only the listeners that are actually up: a URL for a port
/// nobody is accepting on is worse than no URL, because it looks like the
/// feature is on.
fn web_certificates() -> Vec<WebCertificate> {
    crate::web_control::certificates()
        .into_iter()
        .map(|(bind, sha256)| WebCertificate {
            urls: web_urls(&[bind]),
            sha256,
        })
        .collect()
}

fn web_server_status() -> WebServerStatus {
    let listening = crate::web_control::listening();
    let configured = config::configuration()
        .web_servers
        .iter()
        .map(|server| server.bind_address.clone())
        .collect();
    WebServerStatus {
        urls: web_urls(&listening),
        certificates: web_certificates(),
        listening,
        configured,
    }
}

/// The pages a browser can open, one per live listener.
///
/// Built from what is accepting rather than from what is configured, and in
/// both directions: a listener started at runtime is in no `web_servers`
/// entry and would otherwise have no URL at all, and a URL for a configured
/// port nobody is accepting on is worse than none -- it reads as though the
/// feature were on.
fn web_urls(listening: &[String]) -> Vec<String> {
    let config = config::configuration();
    listening
        .iter()
        .flat_map(|address| {
            crate::web_control::effective(address)
                .or_else(|| {
                    config
                        .web_servers
                        .iter()
                        .find(|server| &server.bind_address == address)
                        .cloned()
                })
                .unwrap_or_else(|| config::WebServer {
                    bind_address: address.clone(),
                    ..Default::default()
                })
                .urls()
        })
        .collect()
}

/// The listener a `SetWebServer { enabled: true }` should bring up.
///
/// A configured entry is used whole, so its TLS, origins and bundle
/// directory come with it. An address that is not in the configuration gets
/// the defaults, which is what lets a server with no `web_servers` at all
/// be switched on from a settings window.
///
/// Two of those defaults are borrowed from the first configured entry when
/// there is one, because they describe *this server's* resources rather
/// than how one address is exposed: where the page lives, and where tokens
/// are kept. Without the first, a switch flipped on an ad-hoc port serves
/// no page at all when the bundle is somewhere non-standard.
fn web_server_to_start(bind_address: Option<&str>) -> anyhow::Result<config::WebServer> {
    let config = config::configuration();
    if let Some(address) = bind_address {
        // An address off the wire, so it is checked before it becomes a
        // listener: a typo should fail here, naming itself, rather than as
        // a bind error with no context. A name is as good as a literal --
        // `bind_address` is documented to take either -- but a port is not
        // optional.
        // `split_authority` trims before it parses, so the address has to be
        // checked for being already trimmed as well: what is bound is the
        // string as it arrived, and " 127.0.0.1:8088" would pass the parse
        // and fail the bind, which is the outcome this check is for.
        if address.trim() != address
            || config::split_authority(address).is_none_or(|(_, port)| port.is_none())
        {
            anyhow::bail!("{address:?} is not an address:port to listen on");
        }
        if let Some(server) = config
            .web_servers
            .iter()
            .find(|server| server.bind_address == address)
        {
            return Ok(server.clone());
        }
        let first = config.web_servers.first();
        return Ok(config::WebServer {
            bind_address: address.to_string(),
            static_dir: first.and_then(|s| s.static_dir.clone()),
            token_file: first.and_then(|s| s.token_file.clone()),
            ..Default::default()
        });
    }
    // A server with no `web_servers` at all is the ordinary case now that
    // the switch lives in a settings window, so "on" means the same
    // loopback address a bare `web_servers = { {} }` would have produced
    // rather than an error about configuration the user never wrote.
    Ok(config
        .web_servers
        .first()
        .cloned()
        .unwrap_or_default())
}
