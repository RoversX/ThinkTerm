//! `thinkterm cli share watch`: a pane another account shares with you,
//! drawn in this terminal.
//!
//! The pane is read the way any client reads one -- the owner's server
//! pushes what changed and the rows are fetched as needed -- and drawn at
//! the owner's size: a terminal smaller than that shows the part around
//! the cursor, a larger one leaves the rest blank. Nothing typed goes
//! anywhere until the owner lets this viewer type; until then the keys
//! scroll and pan, and `Ctrl-]` starts a command.
//!
//! The connection runs on a thread of its own and wakes the terminal when
//! something arrives, so an idle viewer costs nothing.

use crate::cli::{NoSuchTarget, UsageError};
use anyhow::Context as _;
use clap::Parser;
use codec::{
    GetLines, GetPaneRenderChanges, GetPaneRenderableDimensions, InputSerial, SendKeyDown,
    SendMouseEvent, SendPaste, ShareJoin, ShareJoinResponse, ShareReleaseControl,
    ShareRequestControl,
};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use termwiz::caps::Capabilities;
use termwiz::cell::{AttributeChange, CellAttributes};
use termwiz::color::ColorAttribute;
use termwiz::input::{InputEvent, KeyCode, KeyEvent, Modifiers, MouseButtons};
use termwiz::surface::{Change, CursorVisibility, Line, Position};
use termwiz::terminal::buffered::BufferedTerminal;
use termwiz::terminal::{Terminal, TerminalWaker};
use wezterm_client::share::{connect, incoming, within, Owner};
use wezterm_client::share_queue::{RenderUpdate, Update};
use wezterm_term::StableRowIndex;

#[derive(Debug, Parser, Clone)]
pub struct WatchCommand {
    /// The share: OWNER/SHARE as `incoming` lists it, or OWNER alone when
    /// that account shares one pane with you. Without it, the one pane
    /// shared with you.
    target: Option<String>,

    /// Print the shared screen once, as text, and exit.
    #[arg(long)]
    once: bool,
}

/// Rows kept around the view; the rest are fetched again if wanted.
const KEEP_ROWS: StableRowIndex = 2000;
/// Rows asked for and not back by then are asked for again.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(3);
/// How often the connection asks after a pane it has heard nothing from:
/// it re-arms the pushes, and finds out if the pane or the share is gone.
const STILL_THERE: Duration = Duration::from_secs(2);
/// User input stays ordered and cannot grow without bound.
const NETWORK_BACKLOG: usize = 16;

impl WatchCommand {
    pub async fn run(&self) -> anyhow::Result<()> {
        // Dropped last, once the terminal is back as it was; an error is
        // logged after, by the caller.
        let _quiet = Quiet::new();
        let (owner, owner_name, share_id) = self.choose().await?;
        if self.once {
            return print_once(&owner, share_id).await;
        }
        watch(owner, owner_name, share_id)
    }

    /// The share the target names, among those shared with us.
    async fn choose(&self) -> anyhow::Result<(Owner, String, u64)> {
        let incoming = incoming().await;
        let found = incoming.found;
        let (want_owner, want_share) = match self.target.as_deref() {
            None => (None, None),
            Some(target) => match target.split_once('/') {
                Some((owner, share)) => (
                    Some(owner.to_string()),
                    Some(share.parse::<u64>().map_err(|_| {
                        UsageError(format!("`{target}`: the share after the / is a number"))
                    })?),
                ),
                None => (Some(target.to_string()), None),
            },
        };
        let mut candidates = vec![];
        for (owner, response) in &found {
            if want_owner
                .as_ref()
                .is_some_and(|want| *want != response.owner)
            {
                continue;
            }
            for share in &response.shares {
                if want_share.is_some_and(|want| want != share.share_id) {
                    continue;
                }
                candidates.push((owner.clone(), response.owner.clone(), share.clone()));
            }
        }
        match candidates.len() {
            1 => {
                let (owner, name, share) = candidates.remove(0);
                Ok((owner, name, share.share_id))
            }
            0 => {
                // Maybe shared, but by a ThinkTerm of another version.
                let other = incoming
                    .other_version
                    .iter()
                    .find(|name| want_owner.as_ref().is_none_or(|want| want == *name));
                Err(NoSuchTarget(match (other, self.target.as_deref()) {
                    (Some(name), _) => format!(
                        "{} runs another version of ThinkTerm: what it shares shows once \
                         both run the same",
                        inert(name)
                    ),
                    (None, Some(target)) => format!("nobody shares {target} with you"),
                    (None, None) => "nobody on this machine shares a pane with you".to_string(),
                })
                .into())
            }
            _ => Err(UsageError(format!(
                "more than one pane is shared with you; pick one:\n{}",
                candidates
                    .iter()
                    .map(|(_, name, share)| format!(
                        "  {}/{}  {}",
                        crate::cli::printable(name),
                        share.share_id,
                        crate::cli::printable(&share.title)
                    ))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
            .into()),
        }
    }
}

/// The screen as it is, as plain text.
async fn print_once(owner: &Owner, share_id: u64) -> anyhow::Result<()> {
    let client = connect(owner).await?;
    let joined = within(STILL_THERE, client.share_join(ShareJoin { share_id })).await?;
    let first = client
        .get_dimensions(GetPaneRenderableDimensions {
            pane_id: joined.pane_id,
        })
        .await?;
    check_dimensions(&first.dimensions)?;
    let dims = first.dimensions;
    let top = dims.physical_top;
    let rows = top..top + dims.viewport_rows as StableRowIndex;
    let (lines, _) = client
        .get_lines(GetLines {
            pane_id: joined.pane_id,
            lines: vec![rows.clone()],
        })
        .await?
        .lines
        .extract_data();
    let by_row: HashMap<StableRowIndex, Line> = lines.into_iter().collect();
    let mut text = String::new();
    for row in rows {
        if let Some(line) = by_row.get(&row) {
            text.push_str(&crate::cli::printable(line.as_str().trim_end()));
        }
        text.push('\n');
    }
    print!("{}", text.trim_end_matches('\n'));
    println!();
    Ok(())
}

/// What the terminal asks the connection to do.
struct RowRequest {
    id: u64,
    ranges: Vec<Range<StableRowIndex>>,
}

enum ToNet {
    Rows(RowRequest),
    Key(KeyEvent),
    Mouse(wezterm_term::input::MouseEvent),
    Paste(String),
    AskToType,
    GiveBack,
}

/// What the connection tells the terminal.
enum FromNet {
    Joined(
        ShareJoinResponse,
        RenderableDimensions,
        StableCursorPosition,
    ),
    Changes(RenderUpdate),
    Rows(u64, Vec<(StableRowIndex, Line)>),
    RetryRows,
    State(codec::ShareStateChanged),
    Note(String),
    Ended(String),
}

fn check_dimensions(dims: &RenderableDimensions) -> anyhow::Result<()> {
    anyhow::ensure!(
        dims.is_plausible(),
        "the share sent invalid terminal dimensions"
    );
    Ok(())
}

struct Commands {
    input: smol::channel::Sender<ToNet>,
    rows: Arc<Mutex<Option<RowRequest>>>,
    wake: smol::channel::Sender<()>,
}

struct CommandReceiver {
    input: smol::channel::Receiver<ToNet>,
    rows: Arc<Mutex<Option<RowRequest>>>,
    wake: smol::channel::Receiver<()>,
}

fn command_channel() -> (Commands, CommandReceiver) {
    let (input, receive) = smol::channel::bounded(NETWORK_BACKLOG);
    let (wake, ready) = smol::channel::bounded(1);
    let rows = Arc::new(Mutex::new(None));
    (
        Commands {
            input,
            rows: Arc::clone(&rows),
            wake,
        },
        CommandReceiver {
            input: receive,
            rows,
            wake: ready,
        },
    )
}

impl Commands {
    fn try_send(&self, command: ToNet) -> Result<Option<u64>, smol::channel::TrySendError<ToNet>> {
        if let ToNet::Rows(ranges) = command {
            let mut rows = self.rows.lock().unwrap();
            if self.wake.is_closed() {
                return Err(smol::channel::TrySendError::Closed(ToNet::Rows(ranges)));
            }
            let replaced = rows.replace(ranges).map(|request| request.id);
            let _ = self.wake.try_send(());
            Ok(replaced)
        } else {
            self.input.try_send(command).map(|()| None)
        }
    }

    fn close(&self) {
        let mut rows = self.rows.lock().unwrap();
        *rows = None;
        self.input.close();
        self.wake.close();
    }
}

impl CommandReceiver {
    async fn recv(&self) -> Result<ToNet, smol::channel::RecvError> {
        smol::future::or(self.input.recv(), async {
            loop {
                self.wake.recv().await?;
                if let Some(rows) = self.rows.lock().unwrap().take() {
                    return Ok(ToNet::Rows(rows));
                }
            }
        })
        .await
    }
}

#[derive(Default)]
struct PendingUpdates {
    joined: Option<(
        ShareJoinResponse,
        RenderableDimensions,
        StableCursorPosition,
    )>,
    render: Option<RenderUpdate>,
    rows: Option<(u64, Vec<(StableRowIndex, Line)>)>,
    retry_rows: bool,
    state: Option<codec::ShareStateChanged>,
    note: Option<String>,
    ended: Option<String>,
}

impl PendingUpdates {
    fn is_empty(&self) -> bool {
        self.joined.is_none()
            && self.render.is_none()
            && self.rows.is_none()
            && !self.retry_rows
            && self.state.is_none()
            && self.note.is_none()
            && self.ended.is_none()
    }
}

#[derive(Default)]
struct InboxState {
    pending: PendingUpdates,
    terminal: bool,
    closed: bool,
}

#[derive(Default)]
struct Inbox(Mutex<InboxState>);

impl Inbox {
    fn post(&self, message: FromNet) -> anyhow::Result<()> {
        let mut inbox = self.0.lock().unwrap();
        anyhow::ensure!(!inbox.closed, "the viewer closed");
        if inbox.terminal {
            return Ok(());
        }
        let dims = match &message {
            FromNet::Joined(_, dims, _) => Some(dims),
            FromNet::Changes(update) => Some(&update.changes.dimensions),
            _ => None,
        };
        if let Some(Err(err)) = dims.map(check_dimensions) {
            inbox.pending = PendingUpdates {
                ended: Some(err.to_string()),
                ..Default::default()
            };
            inbox.terminal = true;
            return Err(err);
        }
        match message {
            FromNet::Joined(joined, dims, cursor) => {
                inbox.pending.joined = Some((joined, dims, cursor))
            }
            FromNet::Changes(update) => match inbox.pending.render.as_mut() {
                Some(previous) => previous.merge(update),
                None => inbox.pending.render = Some(update),
            },
            FromNet::Rows(request, rows) => {
                if inbox.pending.rows.replace((request, rows)).is_some() {
                    inbox.pending.retry_rows = true;
                }
            }
            FromNet::RetryRows => inbox.pending.retry_rows = true,
            FromNet::State(mut state) => {
                if let Some(reason) = state.ended.take() {
                    inbox.pending = PendingUpdates {
                        ended: Some(reason),
                        ..Default::default()
                    };
                    inbox.terminal = true;
                } else {
                    inbox.pending.state = Some(state);
                }
            }
            FromNet::Note(note) => inbox.pending.note = Some(note),
            FromNet::Ended(reason) => {
                inbox.pending = PendingUpdates {
                    ended: Some(reason),
                    ..Default::default()
                };
                inbox.terminal = true;
            }
        }
        Ok(())
    }

    fn close(&self) {
        let mut inbox = self.0.lock().unwrap();
        inbox.closed = true;
        inbox.pending = PendingUpdates::default();
    }
}

struct Connection {
    commands: Commands,
    incoming: Arc<Inbox>,
    stop: smol::channel::Sender<()>,
}

impl Connection {
    fn receive(&self, view: &mut View) -> bool {
        // Take a fixed number of slots so a busy peer leaves time to draw.
        let pending = std::mem::take(&mut self.incoming.0.lock().unwrap().pending);
        if let Some(reason) = pending.ended {
            view.apply(FromNet::Ended(reason));
            return false;
        }
        if let Some((joined, dims, cursor)) = pending.joined {
            view.apply(FromNet::Joined(joined, dims, cursor));
        }
        if let Some(state) = pending.state {
            view.apply(FromNet::State(state));
        }
        if let Some(update) = pending.render {
            view.apply(FromNet::Changes(update));
        }
        if let Some((request, rows)) = pending.rows {
            view.apply(FromNet::Rows(request, rows));
        }
        if pending.retry_rows {
            view.apply(FromNet::RetryRows);
        }
        if let Some(note) = pending.note {
            view.apply(FromNet::Note(note));
        }
        !self.incoming.0.lock().unwrap().pending.is_empty()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.stop.close();
        self.commands.close();
        self.incoming.close();
    }
}

async fn until_stopped<T>(
    stop: smol::channel::Receiver<()>,
    work: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    smol::future::or(
        async {
            let _ = stop.recv().await;
            anyhow::bail!("the viewer closed")
        },
        work,
    )
    .await
}

fn spawn_connection(owner: Owner, share_id: u64, waker: TerminalWaker) -> Connection {
    let (to_net, commands) = command_channel();
    let incoming = Arc::new(Inbox::default());
    let tell = Arc::clone(&incoming);
    let (stop, stopped) = smol::channel::bounded(1);
    std::thread::Builder::new()
        .name("share-watch".into())
        .spawn(move || {
            let send = |message: FromNet| {
                let result = tell.post(message);
                let _ = waker.wake();
                result
            };
            let result = smol::block_on(until_stopped(
                stopped,
                converse(owner, share_id, commands, &send),
            ));
            if let Err(err) = result {
                let _ = send(FromNet::Ended(format!("{err:#}")));
            }
        })
        .expect("starting the share connection thread");
    Connection {
        commands: to_net,
        incoming,
        stop,
    }
}

fn forward_update(
    update: Update,
    send: &impl Fn(FromNet) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    match update {
        Update::Render(update) => send(FromNet::Changes(update)),
        Update::State(state) => send(FromNet::State(state)),
        Update::Ended(reason) => {
            send(FromNet::Ended(reason))?;
            anyhow::bail!("the share ended")
        }
    }
}

async fn with_updates<T, F>(
    work: impl std::future::Future<Output = anyhow::Result<T>>,
    next: impl Fn() -> F,
    send: &impl Fn(FromNet) -> anyhow::Result<()>,
) -> anyhow::Result<T>
where
    F: std::future::Future<Output = Result<Update, smol::channel::RecvError>>,
{
    let mut work = std::pin::pin!(work);
    loop {
        let result = smol::future::or(async { work.as_mut().await.map(Some) }, async {
            forward_update(next().await?, send)?;
            Ok(None)
        })
        .await?;
        if let Some(result) = result {
            return Ok(result);
        }
        smol::future::yield_now().await;
    }
}

async fn converse(
    owner: Owner,
    share_id: u64,
    commands: CommandReceiver,
    send: &impl Fn(FromNet) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let pushes = wezterm_client::client::tap_share_updates();
    let client = connect(&owner).await?;
    let joined = within(STILL_THERE, client.share_join(ShareJoin { share_id })).await?;
    let pane_id = joined.pane_id;
    let first = client
        .get_dimensions(GetPaneRenderableDimensions { pane_id })
        .await?;
    send(FromNet::Joined(
        joined,
        first.dimensions,
        first.cursor_position,
    ))?;
    with_updates(
        client.get_pane_render_changes(GetPaneRenderChanges { pane_id }),
        || pushes.recv(),
        send,
    )
    .await?;

    enum Event {
        Push(Update),
        Command(ToNet),
        StillThere,
        Done,
    }
    loop {
        smol::future::yield_now().await;
        // Service both queues even if either side stays continuously ready.
        if let Ok(update) = pushes.try_recv() {
            forward_update(update, send)?;
        }
        let event = smol::future::or(
            smol::future::or(
                async {
                    match commands.recv().await {
                        Ok(command) => Event::Command(command),
                        Err(_) => Event::Done,
                    }
                },
                async {
                    match pushes.recv().await {
                        Ok(update) => Event::Push(update),
                        Err(_) => Event::Done,
                    }
                },
            ),
            async {
                smol::Timer::after(STILL_THERE).await;
                Event::StillThere
            },
        )
        .await;
        match event {
            Event::Done => anyhow::bail!("the share connection closed"),
            Event::Push(update) => forward_update(update, send)?,
            Event::StillThere => {
                let alive = with_updates(
                    client.get_pane_render_changes(GetPaneRenderChanges { pane_id }),
                    || pushes.recv(),
                    send,
                )
                .await
                .context("the share is no longer there")?;
                if !alive.is_alive {
                    send(FromNet::Ended(codec::share_end::CLOSED.to_string()))?;
                    return Ok(());
                }
            }
            Event::Command(command) => {
                let result: anyhow::Result<()> = with_updates(
                    async {
                        match command {
                            ToNet::Rows(request) => {
                                let (rows, _) = client
                                    .get_lines(GetLines {
                                        pane_id,
                                        lines: request.ranges,
                                    })
                                    .await?
                                    .lines
                                    .extract_data();
                                // Flush earlier pushes before their row reply; each slot is bounded.
                                for _ in 0..3 {
                                    match pushes.try_recv() {
                                        Ok(update) => forward_update(update, send)?,
                                        Err(smol::channel::TryRecvError::Empty) => break,
                                        Err(smol::channel::TryRecvError::Closed) => {
                                            anyhow::bail!("the share connection closed")
                                        }
                                    }
                                }
                                send(FromNet::Rows(request.id, rows))?;
                            }
                            ToNet::Key(event) => {
                                client
                                    .key_down(SendKeyDown {
                                        pane_id,
                                        event,
                                        input_serial: InputSerial::now(),
                                    })
                                    .await?;
                            }
                            ToNet::Mouse(event) => {
                                client
                                    .mouse_event(SendMouseEvent { pane_id, event })
                                    .await?;
                            }
                            ToNet::Paste(data) => {
                                client.send_paste(SendPaste { pane_id, data }).await?;
                            }
                            ToNet::AskToType => {
                                client
                                    .share_request_control(ShareRequestControl { share_id })
                                    .await?;
                            }
                            ToNet::GiveBack => {
                                client
                                    .share_release_control(ShareReleaseControl { share_id })
                                    .await?;
                            }
                        }
                        Ok(())
                    },
                    || pushes.recv(),
                    send,
                )
                .await;
                if let Err(err) = result {
                    send(FromNet::Note(short_error(&err)))?;
                }
            }
        }
    }
}

/// How long a watch that ended in a window's pane waits for the key that
/// closes it.
const ENDED_WAIT: Duration = Duration::from_secs(10 * 60);
/// How often a watch in a window's pane says again where it stands.
const PUBLISH_AGAIN_EVERY: Duration = Duration::from_secs(10);
/// Columns one press of Left or Right pans a narrow view.
const PAN_STEP: isize = 8;

/// Why a watch ended, in words, from the code the share sends. One this
/// build does not know is another account's server's text, and is shown
/// as `inert` leaves it, a line at most.
fn ended_text(code: &str) -> String {
    match code {
        codec::share_end::STOPPED => "the owner stopped sharing".to_string(),
        codec::share_end::REMOVED => "the owner removed you".to_string(),
        codec::share_end::EXPIRED => "the share expired".to_string(),
        codec::share_end::CLOSED => "the shared pane closed".to_string(),
        other => inert(&other.chars().take(120).collect::<String>()),
    }
}

/// Text another account's server sent, for this terminal: no control
/// character in it reaches the terminal as one. Each takes a column, as
/// the cell it stood in did.
fn inert(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

/// A change drawing another account's row, made inert: its text as
/// `inert` leaves it, and no hyperlink, whose address would go out to
/// this terminal as it came.
fn inert_change(change: Change) -> Change {
    match change {
        Change::Text(text) => Change::Text(inert(&text)),
        Change::AllAttributes(mut attrs) => {
            attrs.set_hyperlink(None);
            Change::AllAttributes(attrs)
        }
        other => other,
    }
}

/// `line`'s cells in `columns`, as a line of its own. A wide character
/// keeps the cell after it, so what follows stays in its column; one cut
/// by either edge is left out.
fn columns_of(line: &Line, columns: Range<usize>) -> Line {
    let seqno = line.current_seqno();
    let mut shown = Line::with_width(columns.len(), seqno);
    for cell in line.visible_cells() {
        let start = cell.cell_index();
        if start >= columns.end {
            break;
        }
        if start >= columns.start && start + cell.width().max(1) <= columns.end {
            shown.set_cell(start - columns.start, cell.as_cell(), seqno);
        }
    }
    shown
}

/// The server's reason, without the layers of context around it.
fn short_error(err: &anyhow::Error) -> String {
    let text = err.root_cause().to_string();
    text.strip_prefix("Error: ").unwrap_or(&text).to_string()
}

/// A row asked for and not back yet.
struct Asked {
    /// When it was last asked for; `None` when that ask never went out,
    /// to be made again at once.
    at: Option<Instant>,
    /// The request that last asked for it.
    request: u64,
    /// The first request since the row last changed. The answer to it or
    /// to any later one is the row as it is: asking again does not make an
    /// answer on its way stale.
    since: u64,
}

/// The viewer's picture of the shared pane.
struct View {
    owner: String,
    share_id: u64,
    joined: bool,
    /// Whether the pane's history is shared; without it, only its screen
    /// is, and scrolling back goes over what this viewer has seen.
    history: bool,
    dims: RenderableDimensions,
    cursor: StableCursorPosition,
    title: String,
    mouse_grabbed: bool,
    /// Whether the pane shows its other screen, as last heard.
    alt_screen: Option<bool>,
    render_generation: u64,
    rows: HashMap<StableRowIndex, Line>,
    asked: HashMap<StableRowIndex, Asked>,
    row_request: u64,
    /// Rows scrolled back from the live screen.
    scroll: StableRowIndex,
    /// Columns panned to by hand; `None` follows the cursor.
    pan: Option<usize>,
    controlling: bool,
    requested: bool,
    controller: Option<String>,
    /// `Ctrl-]` was pressed: the next key is a command.
    prefix: bool,
    note: Option<(String, Instant)>,
    ended: Option<String>,
    /// Why watching stopped, shown until a key closes the viewer.
    finished: Option<String>,
    /// Mouse buttons held, to tell a press from a release.
    buttons: MouseButtons,
    /// The ThinkTerm window this runs in shows where the watch stands in
    /// the pane's bar, with a button for asking to type: no status line
    /// here, and the state is published to it as a user variable.
    pane_bar: bool,
    /// What was published last, so each change goes out once.
    published: String,
    /// Shown until the first of the screen comes: the window's words for
    /// it, when it gave them.
    loading: String,
}

impl View {
    fn new(owner: String, share_id: u64, pane_bar: bool) -> Self {
        let loading = match std::env::var("THINKTERM_SHARE_LOADING") {
            Ok(words) if !words.trim().is_empty() => inert(&words),
            _ => format!("Opening {}'s terminal…", inert(&owner)),
        };
        Self {
            owner,
            share_id,
            joined: false,
            history: false,
            dims: RenderableDimensions::default(),
            cursor: StableCursorPosition::default(),
            title: String::new(),
            mouse_grabbed: false,
            alt_screen: None,
            render_generation: 0,
            rows: HashMap::new(),
            asked: HashMap::new(),
            row_request: 0,
            scroll: 0,
            pan: None,
            controlling: false,
            requested: false,
            controller: None,
            prefix: false,
            note: None,
            ended: None,
            finished: None,
            buttons: MouseButtons::NONE,
            pane_bar,
            published: String::new(),
            loading,
        }
    }

    /// The rows above the shared pane's: the status line, unless the
    /// window shows the state in the pane's bar.
    fn header(&self) -> usize {
        usize::from(!self.pane_bar)
    }

    /// Where the watch stands, for the window's pane bar: an OSC 1337 user
    /// variable, `thinkterm_share`, when it has changed since last time.
    fn publication(&mut self) -> Option<String> {
        use base64::Engine as _;
        if !self.pane_bar {
            return None;
        }
        let state = if self.finished.is_some() {
            "ended"
        } else if self.controlling {
            "typing"
        } else if self.requested {
            "asking"
        } else if self.joined {
            "watch"
        } else {
            "connecting"
        };
        // The tab is named for what it shows: the shared pane's title, and
        // whose it is.
        let title = if self.title.is_empty() {
            inert(&self.owner)
        } else {
            format!("{} · {}", inert(&self.title), inert(&self.owner))
        };
        let value = serde_json::json!({
            "state": state,
            "owner": self.owner,
            "why": self.finished,
        })
        .to_string();
        let published = format!("{value}\n{title}");
        if published == self.published {
            return None;
        }
        self.published = published;
        let encoded = base64::engine::general_purpose::STANDARD.encode(value);
        Some(format!(
            "\x1b]2;{title}\x07\x1b]1337;SetUserVar=thinkterm_share={encoded}\x07"
        ))
    }

    fn note(&mut self, text: impl Into<String>) {
        self.note = Some((text.into(), Instant::now()));
    }

    fn send(&mut self, to_net: &Commands, command: ToNet) {
        if self.ended.is_some() {
            return;
        }
        if matches!(&command, ToNet::Paste(text) if text.len() > wezterm_client::share::MAX_RESPONSE_BYTES)
        {
            self.ended = Some("the paste is too large to send to the share".into());
            return;
        }
        match to_net.try_send(command) {
            Ok(Some(replaced)) => {
                // That request never went out: its rows are asked for again.
                for asked in self.asked.values_mut() {
                    if asked.request == replaced {
                        asked.at = None;
                    }
                }
            }
            Ok(None) => {}
            Err(err) => {
                self.ended = Some(
                    match err {
                        smol::channel::TrySendError::Full(_) => {
                            "too much input is waiting for the share"
                        }
                        smol::channel::TrySendError::Closed(_) => "the share connection closed",
                    }
                    .into(),
                )
            }
        }
    }

    /// The earliest row this viewer may scroll to: the pane's first with
    /// its history, else the first it has seen.
    fn earliest(&self) -> StableRowIndex {
        if self.history {
            return self.dims.scrollback_top;
        }
        self.rows
            .keys()
            .copied()
            .min()
            .unwrap_or(self.dims.physical_top)
            .clamp(self.dims.scrollback_top, self.dims.physical_top)
    }

    /// The pane is on `alt_screen`, its screen's top at `physical_top`
    /// now. A view scrolled back stays on its rows while the pane goes
    /// on; the other screen numbers its rows afresh, so nothing kept of
    /// one stands in for the other's.
    fn moved_to(&mut self, alt_screen: bool, physical_top: StableRowIndex) {
        if self.alt_screen.is_some_and(|alt| alt != alt_screen) {
            self.rows.clear();
            self.asked.clear();
            self.go_live();
        } else if self.scroll > 0 {
            self.scroll = (self.scroll as i128 + physical_top as i128
                - self.dims.physical_top as i128)
                .clamp(0, StableRowIndex::MAX as i128) as StableRowIndex;
        }
        self.alt_screen = Some(alt_screen);
    }

    fn apply(&mut self, message: FromNet) {
        if self.ended.is_some() {
            return;
        }
        let dimensions = match &message {
            FromNet::Joined(_, dims, _) => Some(dims),
            FromNet::Changes(changes) => Some(&changes.changes.dimensions),
            _ => None,
        };
        if let Some(Err(err)) = dimensions.map(check_dimensions) {
            self.ended = Some(err.to_string());
            return;
        }
        match message {
            FromNet::Joined(joined, dims, cursor) => {
                self.joined = true;
                self.history = joined.history;
                self.dims = dims;
                self.cursor = cursor;
            }
            FromNet::Changes(update) => {
                let RenderUpdate {
                    changes,
                    carried,
                    screen_changed,
                    generation,
                } = update;
                if generation < self.render_generation {
                    return;
                }
                self.render_generation = generation;
                if screen_changed {
                    self.rows.clear();
                    self.asked.clear();
                    self.go_live();
                }
                let changes = *changes;
                self.moved_to(changes.alt_screen, changes.dimensions.physical_top);
                self.dims = changes.dimensions;
                self.cursor = changes.cursor_position;
                self.title = changes.title;
                self.mouse_grabbed = changes.mouse_grabbed;
                let (bonus, _) = changes.bonus_lines.extract_data();
                // A range from another account may span the whole index
                // space. Visit only cached rows, each at most once, even
                // when the ranges overlap. Bonus rows replace them below.
                let mut cached: BTreeSet<StableRowIndex> =
                    self.rows.keys().chain(self.asked.keys()).copied().collect();
                for range in changes.dirty_lines {
                    if cached.is_empty() {
                        break;
                    }
                    if range.is_empty() {
                        continue;
                    }
                    let dirty: Vec<_> = cached.range(range).copied().collect();
                    for row in dirty {
                        cached.remove(&row);
                        self.rows.remove(&row);
                        self.asked.remove(&row);
                    }
                }
                // What skipped updates brought, then this one's own rows.
                for (row, line) in carried.into_iter().chain(bonus) {
                    self.asked.remove(&row);
                    self.rows.insert(row, line);
                }
            }
            // A reply was lost on the way: what is still out is asked for
            // again, and a late answer to it still counts.
            FromNet::RetryRows => {
                for asked in self.asked.values_mut() {
                    asked.at = None;
                }
            }
            FromNet::Rows(request, rows) => {
                for (row, line) in rows {
                    // A later dirty/bonus update removes this row's request.
                    // Unrelated live output must not starve historical rows.
                    if self
                        .asked
                        .get(&row)
                        .is_some_and(|asked| asked.since <= request)
                    {
                        self.asked.remove(&row);
                        self.rows.insert(row, line);
                    }
                }
                self.asked.retain(|_, asked| asked.request != request);
            }
            FromNet::State(state) => {
                if let Some(reason) = state.ended {
                    self.ended = Some(reason);
                    return;
                }
                if state.controlling && !self.controlling {
                    self.note(format!("{} lets you type. ^] r gives it back.", self.owner));
                    self.go_live();
                } else if !state.controlling && self.controlling {
                    self.note("You can only watch again.");
                }
                self.controlling = state.controlling;
                self.requested = state.requested;
                self.controller = state.controller;
            }
            FromNet::Note(text) => self.note(text),
            FromNet::Ended(reason) => self.ended = Some(reason),
        }
    }

    /// The first row shown at the live screen, for a body `height` rows
    /// tall: the owner's screen from its top or, shorter than that, the
    /// part that keeps the cursor in sight.
    fn live_top(&self, height: usize) -> StableRowIndex {
        let live = self.dims.physical_top;
        let rows = self.dims.viewport_rows as StableRowIndex;
        let height = height as StableRowIndex;
        if height >= rows {
            return live;
        }
        (self.cursor.y as i128 - height as i128 + 1)
            .clamp(live as i128, live as i128 + rows as i128 - height as i128)
            as StableRowIndex
    }

    /// The first row of the shared pane shown, for a body `height` rows
    /// tall: as far back from the live screen as it was scrolled.
    fn top(&self, height: usize) -> StableRowIndex {
        let live_top = self.live_top(height);
        live_top
            .saturating_sub(self.scroll)
            .max(self.earliest().min(live_top))
    }

    /// The first column shown, for a body `width` columns wide: where it
    /// was panned to, else the part that keeps the cursor in sight.
    fn left(&self, width: usize) -> usize {
        if width >= self.dims.cols {
            return 0;
        }
        let most = self.dims.cols - width;
        match self.pan {
            Some(pan) => pan.min(most),
            None if self.scroll > 0 => 0,
            None => self
                .cursor
                .x
                .saturating_add(1)
                .saturating_sub(width)
                .min(most),
        }
    }

    /// Pan the view `columns` to the right, or to the left when negative,
    /// for a body `width` columns wide. It stays there until the view goes
    /// back to the live screen.
    fn pan_by(&mut self, columns: isize, width: usize) {
        if width >= self.dims.cols {
            return;
        }
        let most = (self.dims.cols - width) as isize;
        let from = self.left(width) as isize;
        self.pan = Some(from.saturating_add(columns).clamp(0, most) as usize);
    }

    /// The rows in view that are neither here nor asked for lately. Without
    /// the history, rows off the pane's screen are not given out, and are
    /// not asked for.
    fn missing(&mut self, height: usize) -> Vec<Range<StableRowIndex>> {
        let top = self.top(height);
        let end = top
            .saturating_add(height as StableRowIndex)
            .min(self.dims.physical_top + self.dims.viewport_rows as StableRowIndex);
        let start = if self.history {
            top
        } else {
            top.max(self.dims.physical_top)
        };
        self.asked.retain(|row, _| (start..end).contains(row));
        let mut ranges: Vec<Range<StableRowIndex>> = vec![];
        for row in start..end {
            if self.rows.contains_key(&row) {
                continue;
            }
            let since = match self.asked.get(&row) {
                Some(asked) if asked.at.is_some_and(|at| at.elapsed() < ASK_AGAIN_AFTER) => {
                    continue
                }
                asked => asked.map(|asked| asked.since),
            };
            if ranges.is_empty() {
                let Some(request) = self.row_request.checked_add(1) else {
                    self.ended = Some("too many row requests for this watch".into());
                    return vec![];
                };
                self.row_request = request;
            }
            let request = self.row_request;
            self.asked.insert(
                row,
                Asked {
                    at: Some(Instant::now()),
                    request,
                    since: since.unwrap_or(request),
                },
            );
            match ranges.last_mut() {
                Some(last) if last.end == row => last.end = row + 1,
                _ => ranges.push(row..row + 1),
            }
        }
        // What is far from the view goes; it is asked for again if wanted.
        if self.rows.len() > KEEP_ROWS as usize {
            self.rows
                .retain(|row, _| row.abs_diff(top) < (KEEP_ROWS / 2) as usize);
        }
        ranges
    }

    fn status(&self) -> String {
        let who = format!("{}/{}", inert(&self.owner), self.share_id);
        if let Some(reason) = &self.finished {
            return format!(
                " {who} · stopped watching: {} · press any key to close",
                inert(&ended_text(reason))
            );
        }
        let title = if self.title.is_empty() {
            String::new()
        } else {
            format!(" · {}", inert(&self.title))
        };
        let state = if !self.joined {
            "connecting…".to_string()
        } else if self.controlling {
            "YOU ARE TYPING · ^] r give back · ^] q quit".to_string()
        } else if self.requested {
            format!(
                "asked {} to let you type · ^] r withdraw · ^] q quit",
                self.owner
            )
        } else {
            let typing = match &self.controller {
                Some(name) => format!("{} is typing · ", inert(name)),
                None => String::new(),
            };
            format!("watching · {typing}^] r ask to type · ^] q quit")
        };
        let scroll = if self.scroll > 0 {
            format!(" · {} rows up (End: live)", self.scroll)
        } else {
            String::new()
        };
        let note = match &self.note {
            Some((text, at)) if at.elapsed() < Duration::from_secs(5) => {
                format!(" · {}", inert(text))
            }
            _ => String::new(),
        };
        format!(" {who}{title} · {state}{scroll}{note}")
    }

    fn render(&self, buf: &mut BufferedTerminal<impl Terminal>) {
        let (width, height) = buf.dimensions();
        let header = self.header();
        let body = height.saturating_sub(header);
        buf.add_change(Change::CursorVisibility(CursorVisibility::Hidden));

        // The status line, in reverse, cut to the width.
        if header > 0 {
            let status: String = self.status().chars().take(width).collect();
            buf.add_change(Change::CursorPosition {
                x: Position::Absolute(0),
                y: Position::Absolute(0),
            });
            buf.add_change(Change::AllAttributes(CellAttributes::default()));
            buf.add_change(Change::Attribute(AttributeChange::Reverse(true)));
            buf.add_change(Change::Text(format!("{status:width$}")));
            buf.add_change(Change::AllAttributes(CellAttributes::default()));
        }

        let top = self.top(body);
        let left = self.left(width);
        let end = self.dims.physical_top + self.dims.viewport_rows as StableRowIndex;
        for screen_row in 0..body {
            let row = top.saturating_add(screen_row as StableRowIndex);
            buf.add_change(Change::CursorPosition {
                x: Position::Absolute(0),
                y: Position::Absolute(screen_row + header),
            });
            buf.add_change(Change::AllAttributes(CellAttributes::default()));
            buf.add_change(Change::ClearToEndOfLine(ColorAttribute::Default));
            if row >= end {
                continue;
            }
            if let Some(line) = self.rows.get(&row) {
                let shown = columns_of(line, left..left + width.min(self.dims.cols));
                buf.add_change(Change::CursorPosition {
                    x: Position::Absolute(0),
                    y: Position::Absolute(screen_row + header),
                });
                for change in shown.changes(&CellAttributes::default()) {
                    buf.add_change(inert_change(change));
                }
                buf.add_change(Change::AllAttributes(CellAttributes::default()));
            }
        }

        // Until the first of the screen comes, what is coming, in the
        // middle, dim.
        if self.rows.is_empty() && self.finished.is_none() {
            let text: String = self.loading.chars().take(width).collect();
            let columns = termwiz::cell::unicode_column_width(&text, None);
            buf.add_change(Change::CursorPosition {
                x: Position::Absolute(width.saturating_sub(columns) / 2),
                y: Position::Absolute(header + body / 2),
            });
            buf.add_change(Change::Attribute(AttributeChange::Intensity(
                termwiz::cell::Intensity::Half,
            )));
            buf.add_change(Change::Text(text));
            buf.add_change(Change::AllAttributes(CellAttributes::default()));
            return;
        }

        // The owner's cursor, where it is and while the live screen shows.
        let cursor_row = self.cursor.y.saturating_sub(top);
        let shown = self.scroll == 0
            && self.cursor.visibility == CursorVisibility::Visible
            && (0..body as StableRowIndex).contains(&cursor_row)
            && self.cursor.x >= left
            && self.cursor.x < left + width;
        if shown {
            buf.add_change(Change::CursorPosition {
                x: Position::Absolute(self.cursor.x - left),
                y: Position::Absolute(cursor_row as usize + header),
            });
            buf.add_change(Change::CursorVisibility(CursorVisibility::Visible));
        }
    }

    /// Scroll back `rows`, or forward when negative, for a body `height`
    /// rows tall: up through the owner's screen, then what it has shown.
    fn scroll_by(&mut self, rows: StableRowIndex, height: usize) {
        let most = self.live_top(height).saturating_sub(self.earliest());
        self.scroll = self.scroll.saturating_add(rows).clamp(0, most.max(0));
    }

    /// Back to the live screen, around the cursor.
    fn go_live(&mut self) {
        self.scroll = 0;
        self.pan = None;
    }

    /// A key while watching moves the view; while typing it goes to the
    /// pane. `Ctrl-]` and the key after it are this viewer's own.
    fn key(&mut self, event: KeyEvent, width: usize, body: usize, to_net: &Commands) -> bool {
        let is_escape = matches!(
            (event.key, event.modifiers),
            (KeyCode::Char(']'), Modifiers::CTRL) | (KeyCode::Char('\u{1d}'), _)
        );
        if self.prefix {
            self.prefix = false;
            match event.key {
                KeyCode::Char('q') => return false,
                KeyCode::Char('r') => {
                    let ask = if self.controlling || self.requested {
                        ToNet::GiveBack
                    } else {
                        self.note(format!("Asked {} to let you type.", self.owner));
                        ToNet::AskToType
                    };
                    self.send(to_net, ask);
                }
                _ if is_escape && self.controlling => {
                    self.send(to_net, ToNet::Key(event));
                }
                _ => self.note("^] then q quits, r asks to type or gives it back"),
            }
            return true;
        }
        if is_escape {
            self.prefix = true;
            self.note("^] …");
            return true;
        }
        if self.controlling {
            self.go_live();
            self.send(to_net, ToNet::Key(event));
            return true;
        }
        let page = body.max(1) as StableRowIndex;
        match event.key {
            KeyCode::UpArrow | KeyCode::Char('k') => self.scroll_by(1, body),
            KeyCode::DownArrow | KeyCode::Char('j') => self.scroll_by(-1, body),
            KeyCode::PageUp => self.scroll_by(page, body),
            KeyCode::PageDown => self.scroll_by(-page, body),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_by(StableRowIndex::MAX / 2, body),
            KeyCode::LeftArrow | KeyCode::Char('h') => self.pan_by(-PAN_STEP, width),
            KeyCode::RightArrow | KeyCode::Char('l') => self.pan_by(PAN_STEP, width),
            KeyCode::End | KeyCode::Char('G') => self.go_live(),
            // In a window's pane, closing the tab is how a watch ends: a
            // stray `q` typed at a pane that only watches must not.
            KeyCode::Char('q') if !self.pane_bar => return false,
            _ => self.note(format!(
                "You can only watch. ^] r asks {} to let you type.",
                self.owner
            )),
        }
        true
    }

    /// The wheel scrolls the view, unless the program in a pane we may
    /// type into reads the mouse itself; then the mouse goes to it.
    fn mouse(
        &mut self,
        event: termwiz::input::MouseEvent,
        width: usize,
        body: usize,
        to_net: &Commands,
    ) {
        use wezterm_term::input::{MouseButton, MouseEventKind};
        let wheel_up = event.mouse_buttons.contains(MouseButtons::VERT_WHEEL)
            && event.mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE);
        let wheel_down = event.mouse_buttons.contains(MouseButtons::VERT_WHEEL)
            && !event.mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE);
        let to_pane = self.controlling && self.mouse_grabbed && self.scroll == 0;
        if !to_pane {
            if wheel_up {
                self.scroll_by(3, body);
            } else if wheel_down {
                self.scroll_by(-3, body);
            } else if event.mouse_buttons.contains(MouseButtons::HORZ_WHEEL) {
                let left = event.mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE);
                self.pan_by(if left { -3 } else { 3 }, width);
            }
            return;
        }
        // Where on the owner's screen the event landed. The terminal
        // counts from 1.
        let (event_x, event_y) = (event.x.saturating_sub(1), event.y.saturating_sub(1));
        let header = self.header() as StableRowIndex;
        if (event_y as StableRowIndex) < header {
            return;
        }
        let top = self.top(body);
        let row = top as i128 + event_y as i128 - header as i128 - self.dims.physical_top as i128;
        let x = (event_x as usize).saturating_add(self.left(width));
        if row < 0 || row >= self.dims.viewport_rows as i128 || x >= self.dims.cols {
            return;
        }
        let held = event.mouse_buttons.clone()
            & (MouseButtons::LEFT | MouseButtons::MIDDLE | MouseButtons::RIGHT);
        let was = std::mem::replace(&mut self.buttons, held.clone());
        let (kind, button) = if wheel_up {
            (MouseEventKind::Press, MouseButton::WheelUp(1))
        } else if wheel_down {
            (MouseEventKind::Press, MouseButton::WheelDown(1))
        } else {
            let pressed = held.clone() - was.clone();
            let released = was - held.clone();
            let button_of = |buttons: MouseButtons| {
                if buttons.contains(MouseButtons::LEFT) {
                    MouseButton::Left
                } else if buttons.contains(MouseButtons::MIDDLE) {
                    MouseButton::Middle
                } else if buttons.contains(MouseButtons::RIGHT) {
                    MouseButton::Right
                } else {
                    MouseButton::None
                }
            };
            if !pressed.is_empty() {
                (MouseEventKind::Press, button_of(pressed))
            } else if !released.is_empty() {
                (MouseEventKind::Release, button_of(released))
            } else {
                (MouseEventKind::Move, button_of(held))
            }
        };
        self.send(
            to_net,
            ToNet::Mouse(wezterm_term::input::MouseEvent {
                kind,
                x,
                y: row as i64,
                x_pixel_offset: 0,
                y_pixel_offset: 0,
                button,
                modifiers: event.modifiers,
            }),
        );
    }
}

/// Logging silenced while it lives: a log line would land on the screen
/// drawn here, or among the lines a command prints of its own -- each
/// other account asked is a connection made and let go of, and one of
/// another version is a refused handshake. `WEZTERM_LOG` still says what
/// to log.
pub(crate) struct Quiet(log::LevelFilter);

impl Quiet {
    pub(crate) fn new() -> Self {
        let level = log::max_level();
        if std::env::var_os("WEZTERM_LOG").is_none() {
            log::set_max_level(log::LevelFilter::Off);
        }
        Self(level)
    }
}

impl Drop for Quiet {
    fn drop(&mut self) {
        log::set_max_level(self.0);
    }
}

fn watch(owner: Owner, owner_name: String, share_id: u64) -> anyhow::Result<()> {
    let caps = Capabilities::new_from_env().context("reading the terminal's capabilities")?;
    let mut terminal =
        termwiz::terminal::new_terminal(caps).context("opening this terminal to draw in")?;
    terminal.set_raw_mode()?;
    terminal.enter_alternate_screen()?;
    let mut buf = BufferedTerminal::new(terminal)?;
    let waker = buf.terminal().waker();
    let connection = spawn_connection(owner, share_id, waker);
    let pane_bar = std::env::var("THINKTERM_SHARE_UI").as_deref() == Ok("pane-bar");
    let mut view = View::new(owner_name, share_id, pane_bar);
    let publish = |view: &mut View| -> anyhow::Result<()> {
        if let Some(sequence) = view.publication() {
            use std::io::Write as _;
            let mut out = std::io::stdout();
            out.write_all(sequence.as_bytes())?;
            out.flush()?;
        }
        Ok(())
    };

    let ended = loop {
        let pending = connection.receive(&mut view);
        if let Some(reason) = view.ended.take() {
            break Some(reason);
        }
        let (width, height) = buf.dimensions();
        let body = height.saturating_sub(view.header());
        if view.joined {
            let missing = view.missing(body);
            if !missing.is_empty() {
                view.send(
                    &connection.commands,
                    ToNet::Rows(RowRequest {
                        id: view.row_request,
                        ranges: missing,
                    }),
                );
            }
        }
        if view.ended.is_some() {
            continue;
        }
        view.render(&mut buf);
        buf.flush()?;
        publish(&mut view)?;

        // Until something happens: a key, a resize, or the connection's wake.
        let event = match view.note.as_ref() {
            _ if pending => buf.terminal().poll_input(Some(Duration::ZERO))?,
            // A note fades: look again once it is due to.
            Some((_, at)) => {
                let left = Duration::from_secs(5).saturating_sub(at.elapsed());
                buf.terminal()
                    .poll_input(Some(left.max(Duration::from_millis(50))))?
            }
            // In a window's pane, where the watch stands is said again now
            // and then: a window that restarted has not heard it.
            None if !view.asked.is_empty() => buf.terminal().poll_input(Some(ASK_AGAIN_AFTER))?,
            None if view.pane_bar => buf.terminal().poll_input(Some(PUBLISH_AGAIN_EVERY))?,
            None => buf.terminal().poll_input(None)?,
        };
        if event.is_none() {
            view.published.clear();
        }
        if view
            .note
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= Duration::from_secs(5))
        {
            view.note = None;
        }
        match event {
            Some(InputEvent::Key(key)) => {
                if !view.key(key, width, body, &connection.commands) {
                    break None;
                }
            }
            Some(InputEvent::Mouse(mouse)) => view.mouse(mouse, width, body, &connection.commands),
            Some(InputEvent::Paste(text)) => {
                if view.controlling {
                    view.send(&connection.commands, ToNet::Paste(text));
                } else {
                    view.note("You can only watch; nothing was pasted.");
                }
            }
            Some(InputEvent::Resized { .. }) => {
                buf.check_for_resize()?;
                buf.repaint()?;
                // A window that came back to this pane -- the pane resized
                // to it -- learns where the watch stands again.
                view.published.clear();
            }
            _ => {}
        }
    };
    // The end message can stay on screen for minutes; the connection and
    // its queued input must not stay with it, including a pending RPC.
    drop(connection);
    // The other side ended it: say why on the status line and wait for a
    // key, or a tab opened just to watch would close before it was read.
    if let Some(reason) = &ended {
        view.finished = Some(reason.clone());
        view.render(&mut buf);
        buf.flush()?;
        publish(&mut view)?;
        // In a window's pane, one the window has lost sight of (it restarted,
        // say) closes itself after a while rather than waiting forever.
        let deadline = Instant::now() + ENDED_WAIT;
        loop {
            let wait = view
                .pane_bar
                .then(|| deadline.saturating_duration_since(Instant::now()));
            if wait.is_some_and(|left| left.is_zero()) {
                break;
            }
            match buf.terminal().poll_input(wait)? {
                Some(InputEvent::Key(_)) => break,
                Some(InputEvent::Resized { .. }) => {
                    buf.check_for_resize()?;
                    buf.repaint()?;
                    view.render(&mut buf);
                    buf.flush()?;
                    view.published.clear();
                    publish(&mut view)?;
                }
                _ => {}
            }
        }
    }
    // Letting go of the terminal puts it back as it was.
    drop(buf);
    // The other side ending the watch is an ordinary way for it to end.
    if let Some(reason) = ended {
        eprintln!("Stopped watching: {}.", ended_text(&reason));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codec::{GetPaneRenderChangesResponse, Pdu};
    use std::sync::atomic::{AtomicBool, Ordering};

    fn round_trip(pdu: Pdu) -> Pdu {
        let mut bytes = vec![];
        pdu.encode(&mut bytes, 0).unwrap();
        Pdu::decode(&bytes[..]).unwrap().pdu
    }

    fn join(view: &mut View, dimensions: RenderableDimensions) {
        let Pdu::GetPaneRenderableDimensionsResponse(first) = round_trip(
            Pdu::GetPaneRenderableDimensionsResponse(codec::GetPaneRenderableDimensionsResponse {
                pane_id: 1,
                dimensions,
                cursor_position: Default::default(),
            }),
        ) else {
            panic!("expected dimensions")
        };
        view.apply(FromNet::Joined(
            ShareJoinResponse {
                share_id: 1,
                pane_id: 1,
                viewer_id: 1,
                history: false,
            },
            first.dimensions,
            first.cursor_position,
        ));
    }

    #[test]
    fn invalid_dimensions_end_initial_and_updated_views() {
        let good = RenderableDimensions {
            cols: 80,
            viewport_rows: 24,
            ..Default::default()
        };
        for bad in [
            RenderableDimensions {
                scrollback_top: 1,
                ..good
            },
            RenderableDimensions {
                cols: usize::MAX,
                ..good
            },
            RenderableDimensions {
                viewport_rows: usize::MAX,
                ..good
            },
            RenderableDimensions {
                physical_top: StableRowIndex::MAX,
                ..good
            },
            RenderableDimensions {
                scrollback_top: StableRowIndex::MIN,
                ..good
            },
        ] {
            assert!(check_dimensions(&bad).is_err());
            let mut view = View::new("server-a".into(), 1, false);
            join(&mut view, bad);
            assert!(view.ended.is_some());
            assert!(!view.joined);
            assert_eq!(view.dims, RenderableDimensions::default());

            let mut view = View::new("server-a".into(), 1, false);
            join(&mut view, good);
            assert_eq!(view.missing(24), vec![0..24]);
            let mut update = dirty_update(&view, vec![]);
            update.dimensions = bad;
            let Pdu::GetPaneRenderChangesResponse(update) =
                round_trip(Pdu::GetPaneRenderChangesResponse(update))
            else {
                panic!("expected changes")
            };
            view.apply(FromNet::Changes(RenderUpdate {
                changes: Box::new(update),
                carried: Default::default(),
                screen_changed: false,
                generation: 1,
            }));
            assert!(view.ended.is_some());
            assert_eq!(view.dims, good);
            assert_eq!(view.top(24), 0);
        }
    }

    #[test]
    fn extreme_valid_rows_and_cursors_do_not_overflow() {
        for physical_top in [StableRowIndex::MIN, -4, 0, StableRowIndex::MAX - 24] {
            for cursor_y in [StableRowIndex::MIN, physical_top, StableRowIndex::MAX] {
                let mut view = View::new("server-a".into(), 1, false);
                let dims = RenderableDimensions {
                    cols: 80,
                    viewport_rows: 24,
                    physical_top,
                    scrollback_top: physical_top,
                    ..Default::default()
                };
                join(&mut view, dims);
                assert!(view.ended.is_none());
                view.cursor.y = cursor_y;
                view.cursor.x = usize::MAX;
                assert_eq!(view.left(40), 40);
                for height in [0, 1, 10, 24, 40] {
                    let top = view.top(height);
                    assert!((physical_top..=physical_top + 24).contains(&top));
                    let _ = view.missing(height);
                    view.scroll_by(StableRowIndex::MAX, height);
                    view.scroll_by(StableRowIndex::MAX, height);
                    view.scroll_by(StableRowIndex::MIN, height);
                    assert_eq!(view.scroll, 0);
                }
                for row in 0..=KEEP_ROWS {
                    view.rows
                        .insert(row, Line::from_text("row", &Default::default(), 0, None));
                }
                view.rows
                    .insert(StableRowIndex::MIN, Line::with_width(0, 0));
                view.rows
                    .insert(StableRowIndex::MAX, Line::with_width(0, 0));
                let _ = view.missing(24);
                assert!(view.rows.len() <= KEEP_ROWS as usize);
                view.scroll = 1;
                view.moved_to(false, StableRowIndex::MIN);
                view.scroll = StableRowIndex::MAX;
                view.moved_to(false, StableRowIndex::MAX - 24);
                assert!(view.scroll >= 0);
            }
        }
        let mut view = View::new("server-a".into(), 1, false);
        join(&mut view, RenderableDimensions::default());
        assert!(view.ended.is_none());
        assert!(view.missing(0).is_empty());
    }

    fn channels() -> (
        Connection,
        Arc<Inbox>,
        CommandReceiver,
        smol::channel::Receiver<()>,
    ) {
        let (commands, receive_commands) = command_channel();
        let incoming = Arc::new(Inbox::default());
        let (stop, stopped) = smol::channel::bounded(1);
        (
            Connection {
                commands,
                incoming: Arc::clone(&incoming),
                stop,
            },
            incoming,
            receive_commands,
            stopped,
        )
    }

    #[test]
    fn command_overflow_is_visible_and_oversized_pastes_are_not_queued() {
        let (connection, _, commands, _) = channels();
        let mut view = View::new("server-a".into(), 1, false);
        for _ in 0..NETWORK_BACKLOG {
            view.send(&connection.commands, ToNet::AskToType);
        }
        assert!(view.ended.is_none());
        view.send(&connection.commands, ToNet::GiveBack);
        assert!(view.ended.as_ref().unwrap().contains("too much input"));
        assert_eq!(commands.input.len(), NETWORK_BACKLOG);

        let (connection, _, commands, _) = channels();
        let mut view = View::new("server-a".into(), 1, false);
        view.send(
            &connection.commands,
            ToNet::Paste("x".repeat(wezterm_client::share::MAX_RESPONSE_BYTES + 1)),
        );
        assert!(view.ended.as_ref().unwrap().contains("too large"));
        assert!(commands.input.is_empty());
    }

    #[test]
    fn a_pending_row_request_still_delivers_revoke_and_end_updates() {
        smol::block_on(async {
            let (updates, receive) = smol::channel::bounded(2);
            let (reply, response) = smol::channel::bounded(1);
            updates
                .send(Update::State(codec::ShareStateChanged {
                    share_id: 1,
                    controlling: false,
                    requested: false,
                    controller: None,
                    ended: None,
                }))
                .await
                .unwrap();
            let (connection, inbox, _, _) = channels();
            let send = |message| {
                inbox.post(message)?;
                reply.try_send(()).unwrap();
                Ok(())
            };
            with_updates(
                async {
                    response.recv().await?;
                    Ok(())
                },
                || receive.recv(),
                &send,
            )
            .await
            .unwrap();
            let mut view = View::new("server-a".into(), 1, false);
            view.controlling = true;
            connection.receive(&mut view);
            assert!(!view.controlling);
            assert!(view.ended.is_none());

            updates.send(Update::Ended("stopped".into())).await.unwrap();
            assert!(with_updates(
                std::future::pending::<anyhow::Result<()>>(),
                || receive.recv(),
                &|message| inbox.post(message),
            )
            .await
            .is_err());
            connection.receive(&mut view);
            assert_eq!(view.ended.as_deref(), Some("stopped"));
        });
    }

    #[test]
    fn stopping_the_view_drops_a_pending_network_operation() {
        struct Pending(Arc<AtomicBool>);
        impl Drop for Pending {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        smol::block_on(async {
            let (connection, _, _, stopped) = channels();
            let (started, ready) = smol::channel::bounded(1);
            let dropped = Arc::new(AtomicBool::new(false));
            let work = async {
                let _pending = Pending(Arc::clone(&dropped));
                started.send(()).await.unwrap();
                std::future::pending::<anyhow::Result<()>>().await
            };
            let (result, ()) = smol::future::zip(until_stopped(stopped, work), async {
                ready.recv().await.unwrap();
                drop(connection);
            })
            .await;
            assert!(result.is_err());
            assert!(dropped.load(Ordering::Relaxed));
        });
    }

    fn dirty_update(
        view: &View,
        dirty_lines: Vec<Range<StableRowIndex>>,
    ) -> GetPaneRenderChangesResponse {
        GetPaneRenderChangesResponse {
            pane_id: 1,
            mouse_grabbed: false,
            alt_screen: false,
            keyboard_encoding: Default::default(),
            bracketed_paste: false,
            cursor_position: view.cursor,
            dimensions: view.dims,
            dirty_lines,
            title: String::new(),
            working_dir: None,
            bonus_lines: Vec::new().into(),
            input_serial: None,
            seqno: 1,
        }
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn a_dirty_range_only_visits_cached_rows() {
        let mut view = View::new("server-a".into(), 1, false);
        view.alt_screen = Some(false);
        for row in [StableRowIndex::MIN, -1, 0, 100, StableRowIndex::MAX] {
            view.rows
                .insert(row, Line::from_text("old", &Default::default(), 0, None));
            view.asked.insert(row, asked(0));
        }
        view.asked.insert(200, asked(0));
        view.asked.remove(&-1);
        let mut update = dirty_update(&view, vec![StableRowIndex::MIN..StableRowIndex::MAX]);
        update.bonus_lines = vec![(0, Line::from_text("new", &Default::default(), 1, None))].into();
        // Exercise the real codec too: a small valid frame carries this range.
        let mut bytes = vec![];
        Pdu::GetPaneRenderChangesResponse(update)
            .encode(&mut bytes, 0)
            .unwrap();
        assert!(bytes.len() < 256);
        let Pdu::GetPaneRenderChangesResponse(update) = Pdu::decode(&bytes[..]).unwrap().pdu else {
            panic!("expected a render update");
        };
        view.apply(FromNet::Changes(RenderUpdate {
            changes: Box::new(update),
            carried: Default::default(),
            screen_changed: false,
            generation: 1,
        }));
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.rows[&0].as_str(), "new");
        assert!(view.rows.contains_key(&StableRowIndex::MAX));
        assert_eq!(view.asked.len(), 1);
        assert!(view.asked.contains_key(&StableRowIndex::MAX));
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)]
    fn dirty_ranges_keep_unaffected_rows_and_replace_bonus_rows() {
        let mut view = View::new("server-a".into(), 1, false);
        view.alt_screen = Some(false);
        for row in -4..8 {
            view.rows
                .insert(row, Line::from_text("old", &Default::default(), 0, None));
            view.asked.insert(row, asked(0));
        }
        let mut update = dirty_update(&view, vec![4..6, -3..-1, 1..4, 2..5, 7..7, 7..-4]);
        update.bonus_lines = vec![(2, Line::from_text("new", &Default::default(), 1, None))].into();
        view.apply(FromNet::Changes(RenderUpdate {
            changes: Box::new(update),
            carried: Default::default(),
            screen_changed: false,
            generation: 1,
        }));
        let mut rows: Vec<_> = view.rows.keys().copied().collect();
        rows.sort_unstable();
        assert_eq!(rows, [-4, -1, 0, 2, 6, 7]);
        let mut asked: Vec<_> = view.asked.keys().copied().collect();
        asked.sort_unstable();
        assert_eq!(asked, [-4, -1, 0, 6, 7]);
        assert_eq!(view.rows[&2].as_str(), "new");
    }

    fn asked(request: u64) -> Asked {
        Asked {
            at: Some(Instant::now()),
            request,
            since: request,
        }
    }

    fn view(rows: usize, cursor_y: StableRowIndex) -> View {
        let mut view = View::new("fox".into(), 1, false);
        view.dims = RenderableDimensions {
            cols: 80,
            viewport_rows: rows,
            physical_top: 100,
            scrollback_top: 0,
            ..Default::default()
        };
        view.cursor.y = cursor_y;
        view
    }

    #[test]
    fn an_unknown_reason_is_shown_inert() {
        let shown = ended_text("gone\x1b]2;owned\x07\x1b[2J");
        assert!(!shown.chars().any(|c| c.is_control()), "{:?}", shown);
        assert!(shown.starts_with("gone"));
        assert_eq!(ended_text(&"x".repeat(1000)).chars().count(), 120);
    }

    #[test]
    fn a_short_viewer_follows_the_cursor() {
        // Tall enough: the owner's screen from its top.
        assert_eq!(view(24, 110).top(30), 100);
        // Ten rows for twenty-four: the cursor's row is the last shown...
        assert_eq!(view(24, 115).top(10), 106);
        // ...but never above the screen or past its end.
        assert_eq!(view(24, 101).top(10), 100);
        assert_eq!(view(24, 123).top(10), 114);
    }

    #[test]
    fn without_history_scrolling_goes_over_what_was_seen() {
        let mut v = view(24, 110);
        // Nothing seen above the screen: nowhere to scroll.
        v.scroll_by(1000, 24);
        assert_eq!(v.scroll, 0);
        // Rows seen as they scrolled by can be scrolled back to.
        v.rows
            .insert(90, Line::from_text("seen", &Default::default(), 0, None));
        v.scroll_by(1000, 24);
        assert_eq!(v.scroll, 10);
        assert_eq!(v.top(24), 90);
        // With the history, back to the pane's first row.
        v.history = true;
        v.scroll_by(1000, 24);
        assert_eq!(v.scroll, 100);
    }

    #[test]
    fn without_history_rows_off_the_screen_are_not_asked_for() {
        let mut v = view(4, 100);
        v.joined = true;
        v.rows
            .insert(90, Line::from_text("seen", &Default::default(), 0, None));
        v.scroll_by(5, 4);
        assert!(v.missing(4).is_empty());
        v.scroll = 0;
        assert_eq!(v.missing(4), vec![100..104]);
    }

    #[test]
    fn another_accounts_text_is_inert() {
        assert_eq!(
            inert("a\u{1b}]52;c;eA==\u{7}b"),
            "a\u{fffd}]52;c;eA==\u{fffd}b"
        );
        assert_eq!(inert("plain"), "plain");
    }

    #[test]
    fn a_short_viewer_scrolls_up_through_the_owners_screen() {
        // Fifty rows with the cursor at the bottom, seen through twenty.
        let mut v = view(50, 149);
        assert_eq!(v.top(20), 130);
        // One row up shows the row above, not the screen's top.
        v.scroll_by(1, 20);
        assert_eq!(v.top(20), 129);
        // Without the history, as far as the screen's top, and no further.
        v.scroll_by(1000, 20);
        assert_eq!(v.top(20), 100);
        v.go_live();
        assert_eq!(v.top(20), 130);
    }

    #[test]
    fn a_narrow_viewer_pans_by_hand() {
        let mut v = view(24, 110);
        v.cursor.x = 5;
        assert_eq!(v.left(40), 0);
        // Eighty columns through forty: up to forty from the left.
        v.pan_by(8, 40);
        assert_eq!(v.left(40), 8);
        v.pan_by(1000, 40);
        assert_eq!(v.left(40), 40);
        v.pan_by(-1000, 40);
        assert_eq!(v.left(40), 0);
        // Back to the live screen: around the cursor again.
        v.cursor.x = 70;
        v.go_live();
        assert_eq!(v.left(40), 31);
    }

    #[test]
    fn a_narrow_viewer_pans_to_the_cursor() {
        let mut v = view(24, 110);
        v.cursor.x = 70;
        assert_eq!(v.left(100), 0);
        assert_eq!(v.left(40), 31);
        v.cursor.x = 5;
        assert_eq!(v.left(40), 0);
    }

    #[test]
    fn wide_characters_keep_their_columns() {
        let line = Line::from_text("你好ab中", &Default::default(), 0, None);
        assert_eq!(columns_of(&line, 0..8).as_str(), "你好ab中");
        // One cut by an edge is left out; what follows stays put.
        assert_eq!(columns_of(&line, 1..7).as_str(), " 好ab ");
        assert_eq!(columns_of(&line, 2..5).as_str(), "好a");
    }

    #[test]
    fn a_view_scrolled_back_stays_on_its_rows() {
        let mut v = view(24, 110);
        v.history = true;
        v.moved_to(false, 100);
        v.scroll_by(30, 24);
        assert_eq!(v.top(24), 70);
        // Five rows more of output: the same rows in view.
        v.moved_to(false, 105);
        v.dims.physical_top = 105;
        assert_eq!(v.top(24), 70);
        // At the live screen, the view goes on with it.
        v.scroll = 0;
        v.moved_to(false, 110);
        v.dims.physical_top = 110;
        assert_eq!(v.top(24), 110);
    }

    #[test]
    fn rows_of_the_other_screen_are_not_kept_for_this_one() {
        let mut v = view(24, 110);
        v.moved_to(false, 100);
        v.rows
            .insert(90, Line::from_text("seen", &Default::default(), 0, None));
        // A full-screen program: its screen counts from 0, and the rows
        // kept are not its.
        v.moved_to(true, 0);
        v.dims.physical_top = 0;
        assert!(v.rows.is_empty());
        v.rows
            .insert(0, Line::from_text("vim", &Default::default(), 0, None));
        // Back on the first screen, its row 0 is the history's, not vim's.
        v.moved_to(false, 100);
        v.dims.physical_top = 100;
        assert!(v.rows.is_empty());
        assert_eq!(v.earliest(), 100);
    }

    #[test]
    fn a_click_lands_where_it_was_made() {
        let mut v = view(24, 110);
        v.controlling = true;
        v.mouse_grabbed = true;
        let (tx, rx) = command_channel();
        // The terminal counts from 1: under the status line, the first
        // cell of the owner's screen.
        v.mouse(
            termwiz::input::MouseEvent {
                x: 1,
                y: 2,
                mouse_buttons: MouseButtons::LEFT,
                modifiers: Modifiers::NONE,
            },
            80,
            24,
            &tx,
        );
        match rx.input.try_recv() {
            Ok(ToNet::Mouse(event)) => assert_eq!((event.x, event.y), (0, 0)),
            _ => panic!("the click was not sent"),
        }
    }

    #[test]
    fn missing_rows_are_asked_for_once() {
        let mut v = view(4, 100);
        v.joined = true;
        assert_eq!(v.missing(10), vec![100..104]);
        assert!(v.missing(10).is_empty());
    }

    fn render_update(view: &View, generation: u64) -> RenderUpdate {
        RenderUpdate::new(dirty_update(view, vec![]), generation).unwrap()
    }

    fn text_line(text: &str) -> Line {
        Line::from_text(text, &Default::default(), 0, None)
    }

    #[test]
    fn a_slow_viewer_keeps_the_latest_of_thousands_of_updates_and_control_state() {
        let (connection, incoming, _, _) = channels();
        let mut v = view(4, 100);
        incoming
            .post(FromNet::Joined(
                ShareJoinResponse {
                    share_id: 1,
                    pane_id: 1,
                    viewer_id: 1,
                    history: false,
                },
                v.dims,
                v.cursor,
            ))
            .unwrap();
        incoming
            .post(FromNet::State(codec::ShareStateChanged {
                share_id: 1,
                controlling: true,
                requested: false,
                controller: Some("viewer-a".into()),
                ended: None,
            }))
            .unwrap();
        for generation in 1..=5000 {
            let mut update = render_update(&v, generation);
            update.changes.title = format!("frame {generation}");
            update.changes.cursor_position.x = (generation % 80) as usize;
            update.changes.bonus_lines =
                vec![(100, text_line(&format!("row {generation}")))].into();
            incoming.post(FromNet::Changes(update)).unwrap();
        }
        {
            let inbox = incoming.0.lock().unwrap();
            let update = inbox.pending.render.as_ref().unwrap();
            assert_eq!(update.generation, 5000);
            assert_eq!(update.changes.bonus_lines.rows().count(), 1);
            assert!(inbox.pending.state.is_some());
            assert!(inbox.pending.ended.is_none());
            assert!(!inbox.closed);
            assert!(!inbox.terminal);
        }
        assert!(!connection.receive(&mut v));
        assert!(v.joined);
        assert!(v.controlling);
        assert_eq!(v.controller.as_deref(), Some("viewer-a"));
        assert_eq!(v.render_generation, 5000);
        assert_eq!(v.title, "frame 5000");
        assert_eq!(v.cursor.x, 40);
        assert_eq!(v.rows[&100].as_str(), "row 5000");
        assert!(v.ended.is_none());
        assert!(incoming.0.lock().unwrap().pending.is_empty());
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn skipped_updates_keep_the_rows_they_brought_and_only_changed_rows_are_fetched() {
        let (connection, incoming, _, _) = channels();
        let mut v = view(5, 100);
        v.joined = true;
        v.history = true;
        v.alt_screen = Some(false);
        for row in [90, 100, 101, 102, 103, 104] {
            v.rows.insert(row, text_line("old"));
            v.asked.insert(row, asked(0));
        }
        let mut skipped = render_update(&v, 1);
        skipped.changes.dirty_lines.push(100..101);
        skipped.changes.bonus_lines = vec![(102, text_line("skipped"))].into();
        incoming.post(FromNet::Changes(skipped)).unwrap();
        let mut latest = render_update(&v, 2);
        latest.changes.dirty_lines.push(103..104);
        latest.changes.bonus_lines = vec![(103, text_line("latest"))].into();
        incoming.post(FromNet::Changes(latest)).unwrap();

        assert!(!connection.receive(&mut v));
        assert_eq!(v.rows[&90].as_str(), "old");
        assert_eq!(v.rows[&101].as_str(), "old");
        assert_eq!(v.rows[&102].as_str(), "skipped");
        assert_eq!(v.rows[&103].as_str(), "latest");
        assert_eq!(v.rows[&104].as_str(), "old");
        assert!(!v.rows.contains_key(&100));
        assert!(!v.asked.contains_key(&100));
        assert_eq!(v.missing(5), vec![100..101]);
        assert!(v.missing(5).is_empty());
        assert!(v.ended.is_none());
    }

    #[test]
    fn rows_between_coalesced_changes_stay_and_their_answers_count() {
        let (connection, incoming, _, _) = channels();
        let mut v = view(24, 123);
        v.joined = true;
        v.alt_screen = Some(false);
        assert_eq!(v.missing(24), vec![100..124]);
        let request = v.row_request;
        let mut generation = 0;
        for round in 0..5 {
            // A clock on the top row and the cursor's row at the bottom,
            // twice before the viewer looks.
            for _ in 0..2 {
                generation += 1;
                let mut update = render_update(&v, generation);
                update.changes.bonus_lines =
                    vec![(100, text_line("clock")), (123, text_line("$ "))].into();
                incoming.post(FromNet::Changes(update)).unwrap();
            }
            if round == 0 {
                incoming
                    .post(FromNet::Rows(
                        request,
                        (101..123).map(|row| (row, text_line("static"))).collect(),
                    ))
                    .unwrap();
            }
            assert!(!connection.receive(&mut v));
            assert_eq!(v.rows.len(), 24);
        }
        assert_eq!(v.rows[&110].as_str(), "static");
        assert!(v.asked.is_empty());
        assert!(v.missing(24).is_empty());
    }

    #[test]
    fn an_answer_slower_than_its_retries_still_fills_the_rows() {
        let (connection, incoming, _, _) = channels();
        let mut v = view(4, 100);
        v.joined = true;
        assert_eq!(v.missing(4), vec![100..104]);
        let first = v.row_request;
        for _ in 0..4 {
            // Nothing back in time: the rows are asked for again.
            for asked in v.asked.values_mut() {
                asked.at = Instant::now().checked_sub(ASK_AGAIN_AFTER);
            }
            assert_eq!(v.missing(4), vec![100..104]);
        }
        assert_ne!(v.row_request, first);
        incoming
            .post(FromNet::Rows(
                first,
                (100..104).map(|row| (row, text_line("current"))).collect(),
            ))
            .unwrap();
        assert!(!connection.receive(&mut v));
        assert_eq!(v.rows.len(), 4);
        assert!(v.asked.is_empty());
        assert!(v.missing(4).is_empty());
    }

    #[test]
    fn a_coalesced_round_trip_through_the_other_screen_invalidates_cached_rows() {
        let (connection, incoming, _, _) = channels();
        let mut v = view(4, 100);
        v.joined = true;
        v.history = true;
        v.alt_screen = Some(false);
        v.scroll = 10;
        v.pan = Some(5);
        v.rows.insert(90, text_line("history"));
        v.rows.insert(100, text_line("old screen"));
        v.asked.insert(101, asked(0));
        for (generation, alt_screen) in [(1, false), (2, true), (3, false)] {
            let mut update = render_update(&v, generation);
            update.changes.alt_screen = alt_screen;
            if generation == 3 {
                update.changes.bonus_lines = vec![(100, text_line("new screen"))].into();
            }
            incoming.post(FromNet::Changes(update)).unwrap();
        }

        assert!(!connection.receive(&mut v));
        assert_eq!(v.alt_screen, Some(false));
        assert_eq!(v.scroll, 0);
        assert_eq!(v.pan, None);
        assert_eq!(v.rows.len(), 1);
        assert_eq!(v.rows[&100].as_str(), "new screen");
        assert!(v.asked.is_empty());
        assert_eq!(v.missing(4), vec![101..104]);
    }

    #[test]
    fn dirty_and_bonus_updates_reject_affected_reply_rows_but_keep_unaffected_rows() {
        let (connection, incoming, _, _) = channels();
        let mut v = view(4, 100);
        v.joined = true;
        assert_eq!(v.missing(4), vec![100..104]);
        let original_request = v.row_request;
        let mut latest = render_update(&v, 2);
        latest.changes.dirty_lines.push(100..101);
        latest.changes.bonus_lines = vec![(101, text_line("latest"))].into();
        incoming.post(FromNet::Changes(latest)).unwrap();
        incoming
            .post(FromNet::Rows(
                original_request,
                vec![
                    (100, text_line("stale dirty row")),
                    (101, text_line("stale bonus row")),
                    (102, text_line("unaffected")),
                ],
            ))
            .unwrap();

        assert!(!connection.receive(&mut v));
        assert!(!v.rows.contains_key(&100));
        assert_eq!(v.rows[&101].as_str(), "latest");
        assert_eq!(v.rows[&102].as_str(), "unaffected");
        assert!(v.asked.is_empty());
        assert_eq!(v.missing(4), vec![100..101, 103..104]);
        let repair_request = v.row_request;
        assert_ne!(repair_request, original_request);
        incoming
            .post(FromNet::Rows(
                original_request,
                vec![(100, text_line("old reply")), (103, text_line("old reply"))],
            ))
            .unwrap();
        assert!(!connection.receive(&mut v));
        assert!(!v.rows.contains_key(&100));
        assert!(!v.rows.contains_key(&103));
        assert_eq!(v.asked[&100].request, repair_request);
        assert_eq!(v.asked[&103].request, repair_request);
        incoming
            .post(FromNet::Rows(
                repair_request,
                vec![(100, text_line("current")), (103, text_line("current"))],
            ))
            .unwrap();
        assert!(!connection.receive(&mut v));
        assert_eq!(v.rows[&100].as_str(), "current");
        assert_eq!(v.rows[&103].as_str(), "current");
        assert!(v.asked.is_empty());
        assert!(v.missing(4).is_empty());
        assert!(v.ended.is_none());
    }

    #[test]
    fn replaced_row_replies_are_requested_again_without_waiting_for_a_timeout() {
        let (connection, incoming, _, _) = channels();
        let mut v = view(4, 100);
        v.joined = true;
        assert_eq!(v.missing(2), vec![100..102]);
        let replaced_request = v.row_request;
        assert_eq!(v.missing(4), vec![102..104]);
        let kept_request = v.row_request;
        incoming
            .post(FromNet::Rows(
                replaced_request,
                vec![(100, text_line("replaced"))],
            ))
            .unwrap();
        incoming
            .post(FromNet::Rows(kept_request, vec![(102, text_line("kept"))]))
            .unwrap();
        assert!(!connection.receive(&mut v));
        assert_eq!(v.rows[&102].as_str(), "kept");
        assert_eq!(v.asked.len(), 2);
        assert!(v.asked.values().all(|asked| asked.at.is_none()));
        assert_eq!(v.missing(4), vec![100..102, 103..104]);
        assert!(v.ended.is_none());
    }

    #[test]
    fn enlarging_the_view_retries_rows_from_a_replaced_request_immediately() {
        let (connection, incoming, commands, _) = channels();
        let mut v = view(8, 100);
        v.joined = true;
        let first = v.missing(4);
        assert_eq!(first, vec![100..104]);
        let replaced_request = v.row_request;
        v.send(
            &connection.commands,
            ToNet::Rows(RowRequest {
                id: replaced_request,
                ranges: first,
            }),
        );
        let enlarged = v.missing(8);
        assert_eq!(enlarged, vec![104..108]);
        let latest_request = v.row_request;
        v.send(
            &connection.commands,
            ToNet::Rows(RowRequest {
                id: latest_request,
                ranges: enlarged,
            }),
        );
        for row in 100..104 {
            assert!(v.asked[&row].at.is_none());
        }
        for row in 104..108 {
            assert_eq!(v.asked[&row].request, latest_request);
        }
        let ToNet::Rows(request) = smol::block_on(commands.recv()).unwrap() else {
            panic!("expected the latest row request");
        };
        assert_eq!(request.id, latest_request);
        assert_eq!(request.ranges, vec![104..108]);
        incoming
            .post(FromNet::Rows(
                request.id,
                (104..108).map(|row| (row, text_line("expanded"))).collect(),
            ))
            .unwrap();
        assert!(!connection.receive(&mut v));
        let repair = v.missing(8);
        assert_eq!(repair, vec![100..104]);
        let repair_request = v.row_request;
        v.send(
            &connection.commands,
            ToNet::Rows(RowRequest {
                id: repair_request,
                ranges: repair,
            }),
        );
        let ToNet::Rows(request) = smol::block_on(commands.recv()).unwrap() else {
            panic!("expected the repair request");
        };
        assert_eq!(request.id, repair_request);
        assert_eq!(request.ranges, vec![100..104]);
        incoming
            .post(FromNet::Rows(
                request.id,
                (100..104).map(|row| (row, text_line("repaired"))).collect(),
            ))
            .unwrap();
        assert!(!connection.receive(&mut v));
        assert!(v.missing(8).is_empty());
        assert_eq!(v.rows.len(), 8);
        assert!(v.ended.is_none());
    }

    #[test]
    fn unrelated_live_updates_do_not_starve_an_in_flight_history_request() {
        let (connection, incoming, commands, _) = channels();
        let mut v = view(4, 100);
        v.joined = true;
        v.history = true;
        v.alt_screen = Some(false);
        v.scroll = 10;
        let missing = v.missing(4);
        assert_eq!(missing, vec![90..94]);
        v.send(
            &connection.commands,
            ToNet::Rows(RowRequest {
                id: v.row_request,
                ranges: missing,
            }),
        );
        let ToNet::Rows(request) = smol::block_on(commands.recv()).unwrap() else {
            panic!("expected the history request");
        };
        for generation in 1..=5000 {
            let mut update = render_update(&v, generation);
            update.changes.dirty_lines.push(100..104);
            update.changes.bonus_lines = vec![(100, text_line("live output"))].into();
            incoming.post(FromNet::Changes(update)).unwrap();
        }
        incoming
            .post(FromNet::Rows(
                request.id,
                (90..94).map(|row| (row, text_line("history"))).collect(),
            ))
            .unwrap();

        assert!(!connection.receive(&mut v));
        assert_eq!(v.render_generation, 5000);
        assert_eq!(v.top(4), 90);
        for row in 90..94 {
            assert_eq!(v.rows[&row].as_str(), "history");
        }
        assert_eq!(v.rows[&100].as_str(), "live output");
        assert!(v.asked.is_empty());
        assert!(v.missing(4).is_empty());
        assert!(v.ended.is_none());
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn row_request_bursts_keep_the_latest_request_and_preserve_input_order() {
        let (commands, receiver) = command_channel();
        let mut v = view(4, 100);
        v.send(&commands, ToNet::AskToType);
        for row in 0..5000 {
            v.send(
                &commands,
                ToNet::Rows(RowRequest {
                    id: row as u64 + 1,
                    ranges: vec![row..row + 4],
                }),
            );
            if row == 100 {
                v.send(&commands, ToNet::Paste("input".into()));
            }
        }
        v.send(&commands, ToNet::GiveBack);
        assert!(v.ended.is_none());
        assert_eq!(receiver.input.len(), 3);
        assert_eq!(receiver.wake.len(), 1);
        {
            let pending = receiver.rows.lock().unwrap();
            let request = pending.as_ref().unwrap();
            assert_eq!(request.id, 5000);
            assert_eq!(request.ranges, vec![4999..5003]);
        }

        let mut inputs = vec![];
        let mut row_requests = 0;
        smol::block_on(async {
            for _ in 0..4 {
                match receiver.recv().await.unwrap() {
                    ToNet::Rows(rows) => {
                        assert_eq!(rows.id, 5000);
                        assert_eq!(rows.ranges, vec![4999..5003]);
                        row_requests += 1;
                    }
                    ToNet::AskToType => inputs.push("ask"),
                    ToNet::Paste(text) => {
                        assert_eq!(text, "input");
                        inputs.push("paste");
                    }
                    ToNet::GiveBack => inputs.push("release"),
                    _ => panic!("unexpected input"),
                }
            }
        });
        assert_eq!(inputs, ["ask", "paste", "release"]);
        assert_eq!(row_requests, 1);
        assert!(receiver.input.is_empty());
        assert!(receiver.rows.lock().unwrap().is_none());
    }

    #[test]
    fn an_end_discards_pending_updates_and_cannot_be_replaced() {
        for state_ends in [false, true] {
            let (connection, incoming, _, _) = channels();
            let mut v = view(4, 100);
            incoming
                .post(FromNet::Changes(render_update(&v, 1)))
                .unwrap();
            incoming
                .post(FromNet::Rows(1, vec![(100, text_line("pending"))]))
                .unwrap();
            incoming.post(FromNet::RetryRows).unwrap();
            incoming.post(FromNet::Note("pending".into())).unwrap();
            let state = codec::ShareStateChanged {
                share_id: 1,
                controlling: true,
                requested: false,
                controller: Some("viewer-a".into()),
                ended: state_ends.then(|| "ended first".into()),
            };
            incoming.post(FromNet::State(state)).unwrap();
            if !state_ends {
                incoming.post(FromNet::Ended("ended first".into())).unwrap();
            }
            incoming
                .post(FromNet::Changes(render_update(&v, 2)))
                .unwrap();
            incoming.post(FromNet::Ended("ended later".into())).unwrap();
            {
                let inbox = incoming.0.lock().unwrap();
                assert!(inbox.terminal);
                assert!(inbox.pending.render.is_none());
                assert!(inbox.pending.rows.is_none());
                assert!(inbox.pending.state.is_none());
                assert!(inbox.pending.note.is_none());
                assert!(!inbox.pending.retry_rows);
            }
            assert!(!connection.receive(&mut v));
            assert_eq!(v.ended.as_deref(), Some("ended first"));
            assert_eq!(v.render_generation, 0);
            assert!(!v.controlling);
            assert!(v.rows.is_empty());
            assert!(v.note.is_none());
            assert!(incoming.0.lock().unwrap().pending.is_empty());
        }
    }

    #[test]
    fn invalid_dimensions_end_the_inbox_before_later_valid_updates() {
        for bad_join in [false, true] {
            let (connection, incoming, _, _) = channels();
            let mut v = view(4, 100);
            let bad = RenderableDimensions {
                cols: usize::MAX,
                ..v.dims
            };
            let invalid = if bad_join {
                FromNet::Joined(
                    ShareJoinResponse {
                        share_id: 1,
                        pane_id: 1,
                        viewer_id: 1,
                        history: false,
                    },
                    bad,
                    v.cursor,
                )
            } else {
                let mut update = render_update(&v, 1);
                update.changes.dimensions = bad;
                FromNet::Changes(update)
            };
            assert!(incoming.post(invalid).is_err());
            incoming
                .post(FromNet::Changes(render_update(&v, 2)))
                .unwrap();
            incoming.post(FromNet::Note("later".into())).unwrap();
            assert!(!connection.receive(&mut v));
            assert!(v
                .ended
                .as_ref()
                .unwrap()
                .contains("invalid terminal dimensions"));
            assert_eq!(v.render_generation, 0);
            assert!(!v.joined);
            assert!(v.note.is_none());
            let inbox = incoming.0.lock().unwrap();
            assert!(inbox.terminal);
            assert!(inbox.pending.is_empty());
        }
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn dropping_the_connection_releases_pending_payloads_and_closes_channels() {
        let (connection, incoming, commands, stopped) = channels();
        let mut v = view(4, 100);
        incoming
            .post(FromNet::Changes(render_update(&v, 1)))
            .unwrap();
        incoming
            .post(FromNet::Rows(1, vec![(100, text_line("pending"))]))
            .unwrap();
        incoming.post(FromNet::Note("pending".into())).unwrap();
        v.send(
            &connection.commands,
            ToNet::Rows(RowRequest {
                id: 1,
                ranges: vec![100..104],
            }),
        );
        drop(connection);

        {
            let inbox = incoming.0.lock().unwrap();
            assert!(inbox.closed);
            assert!(inbox.pending.is_empty());
        }
        assert!(commands.input.is_closed());
        assert!(commands.wake.is_closed());
        assert!(commands.rows.lock().unwrap().is_none());
        assert!(matches!(
            stopped.try_recv(),
            Err(smol::channel::TryRecvError::Closed)
        ));
        assert!(incoming.post(FromNet::Note("after close".into())).is_err());
        assert!(incoming.0.lock().unwrap().pending.is_empty());
    }
}
