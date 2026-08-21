use crate::domain::DomainId;
use crate::termwiztermtab;
use crate::Mux;
use anyhow::{anyhow, bail, Context as _};
use crossbeam::channel::{unbounded, Receiver, Sender};
use finl_unicode::grapheme_clusters::Graphemes;
use promise::spawn::block_on;
use promise::Promise;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use termwiz::cell::{unicode_column_width, CellAttributes};
use termwiz::lineedit::*;
use termwiz::surface::{Change, Position};
use termwiz::terminal::*;
use wezterm_term::TerminalSize;

#[derive(Default)]
struct PasswordPromptHost {
    history: BasicHistory,
}
impl LineEditorHost for PasswordPromptHost {
    fn history(&mut self) -> &mut dyn History {
        &mut self.history
    }

    // Rewrite the input so that we can obscure the password
    // characters when output to the terminal widget
    fn highlight_line(&self, line: &str, cursor_position: usize) -> (Vec<OutputElement>, usize) {
        let placeholder = "🔑";
        let grapheme_count = unicode_column_width(line, None);
        let mut output = vec![];
        for _ in 0..grapheme_count {
            output.push(OutputElement::Text(placeholder.to_string()));
        }
        (
            output,
            unicode_column_width(placeholder, None) * cursor_position,
        )
    }
}

pub enum UIRequest {
    /// Display something
    Output(Vec<Change>),
    /// Request input
    Input {
        prompt: String,
        echo: bool,
        respond: Promise<String>,
    },
    /// Sleep with a progress bar
    Sleep {
        reason: String,
        duration: Duration,
        respond: Promise<()>,
    },
    /// Become visible without reading anything back. Only the lazy relay acts
    /// on this; every other backend is already visible or already headless.
    Materialize,
    Close,
}

struct ConnectionUIImpl {
    term: termwiztermtab::TermWizTerminal,
    rx: Receiver<UIRequest>,
}

#[derive(PartialEq, Eq)]
enum CloseStatus {
    Explicit,
    Implicit,
}

impl ConnectionUIImpl {
    fn run(&mut self) -> anyhow::Result<CloseStatus> {
        loop {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(UIRequest::Close) => return Ok(CloseStatus::Explicit),
                Ok(UIRequest::Output(changes)) => self.term.render(&changes)?,
                Ok(UIRequest::Input {
                    prompt,
                    echo: true,
                    mut respond,
                }) => {
                    respond.result(self.input_prompt(&prompt));
                }
                Ok(UIRequest::Input {
                    prompt,
                    echo: false,
                    mut respond,
                }) => {
                    respond.result(self.password_prompt(&prompt));
                }
                Ok(UIRequest::Sleep {
                    reason,
                    duration,
                    mut respond,
                }) => {
                    respond.result(self.sleep(&reason, duration));
                }
                // Already visible.
                Ok(UIRequest::Materialize) => {}
                Err(err) if err.is_timeout() => {}
                Err(err) => bail!("recv_timeout: {}", err),
            }
        }
    }

    fn password_prompt(&mut self, prompt: &str) -> anyhow::Result<String> {
        let mut editor = LineEditor::new(&mut self.term);
        editor.set_prompt(prompt);

        let mut host = PasswordPromptHost::default();
        if let Some(line) = editor.read_line(&mut host)? {
            Ok(line)
        } else {
            bail!("password entry was cancelled");
        }
    }

    fn input_prompt(&mut self, prompt: &str) -> anyhow::Result<String> {
        let mut editor = LineEditor::new(&mut self.term);
        editor.set_prompt(prompt);

        let mut host = NopLineEditorHost::default();
        if let Some(line) = editor.read_line(&mut host)? {
            Ok(line)
        } else {
            bail!("prompt cancelled");
        }
    }

    fn sleep(&mut self, reason: &str, duration: Duration) -> anyhow::Result<()> {
        let start = Instant::now();
        let deadline = start + duration;
        let mut last_draw = None;

        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }

            // Render a progress bar underneath the countdown text by reversing
            // out the text for the elapsed portion of time.
            let remain = deadline - now;
            let term_width = self.term.get_screen_size().map(|s| s.cols).unwrap_or(80);
            let prog_width = term_width as u128 * (duration.as_millis() - remain.as_millis())
                / duration.as_millis();
            let prog_width = prog_width as usize;
            let message = format!("{} ({:.0?})", reason, remain);

            let mut reversed_string = String::new();
            let mut default_string = String::new();
            let mut col = 0;
            for grapheme in Graphemes::new(&message) {
                // Once we've passed the elapsed column, full up the string
                // that we'll render with default attributes instead.
                if col > prog_width {
                    default_string.push_str(grapheme);
                } else {
                    reversed_string.push_str(grapheme);
                }
                col += 1;
            }

            // If we didn't reach the elapsed column yet (really short text!),
            // we need to pad out the reversed string.
            while col < prog_width {
                reversed_string.push(' ');
                col += 1;
            }

            let combined = format!("{}{}", reversed_string, default_string);

            if last_draw.is_none() || last_draw.as_ref().unwrap() != &combined {
                self.term.render(&[
                    Change::CursorPosition {
                        x: Position::Absolute(0),
                        y: Position::Relative(0),
                    },
                    Change::AllAttributes(CellAttributes::default().set_reverse(true).clone()),
                    Change::Text(reversed_string),
                    Change::AllAttributes(CellAttributes::default()),
                    Change::Text(default_string),
                ])?;
                last_draw.replace(combined);
            }

            // We use poll_input rather than a raw sleep here so that
            // eg: resize events can be processed and reflected in the
            // dimensions reported at the top of the loop.
            // We're using a sub-second value for the delay here for a
            // slightly smoother progress bar.
            self.term
                .poll_input(Some(remain.min(Duration::from_millis(50))))?;
        }

        let message = format!("{} (done)\r\n", reason);
        self.term.render(&[
            Change::CursorPosition {
                x: Position::Absolute(0),
                y: Position::Relative(0),
            },
            Change::Text(message),
        ])?;

        Ok(())
    }
}

struct HeadlessImpl {
    rx: Receiver<UIRequest>,
}

impl HeadlessImpl {
    fn run(&mut self) -> anyhow::Result<()> {
        loop {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(UIRequest::Close) => break,
                Ok(UIRequest::Output(changes)) => {
                    log::trace!("Output: {:?}", changes);
                }
                Ok(UIRequest::Input { mut respond, .. }) => {
                    respond.result(Err(anyhow!("Input requested from headless context")));
                }
                Ok(UIRequest::Sleep {
                    mut respond,
                    reason,
                    duration,
                }) => {
                    log::error!("{} (sleeping for {:?})", reason, duration);
                    std::thread::sleep(duration);
                    respond.result(Ok(()));
                }
                // Nothing to become visible in.
                Ok(UIRequest::Materialize) => {}
                Err(err) if err.is_timeout() => {}
                Err(err) => bail!("recv_timeout: {}", err),
            }
        }

        Ok(())
    }
}

/// Everything the silent phase printed, kept so that a window materialized
/// later can show the user *why* they are being asked for a password.
///
/// Bounded: a long outage prints an unbounded number of "problem reconnecting"
/// lines, and the relay may live for hours. The tail is what explains the
/// prompt, so trimming drops from the front.
#[derive(Default)]
struct OutputBacklog {
    changes: VecDeque<Change>,
    bytes: usize,
    /// The most recent title, held out of the ring so trimming can never
    /// silently drop it.
    title: Option<String>,
    truncated: bool,
}

impl OutputBacklog {
    const MAX_CHANGES: usize = 512;
    const MAX_BYTES: usize = 32 * 1024;

    fn push(&mut self, change: Change) {
        if let Change::Title(title) = &change {
            self.title = Some(title.clone());
            return;
        }
        self.bytes += Self::cost(&change);
        self.changes.push_back(change);
        while self.changes.len() > Self::MAX_CHANGES || self.bytes > Self::MAX_BYTES {
            let Some(dropped) = self.changes.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(Self::cost(&dropped));
            self.truncated = true;
        }
    }

    fn cost(change: &Change) -> usize {
        match change {
            Change::Text(s) => s.len().max(1),
            _ => 1,
        }
    }

    /// What to render into a freshly materialized window, in order.
    /// `for_input` selects the closing guidance: a window opened for a
    /// prompt asks the user to type, one opened just to show something
    /// (e.g. a fatal host-key warning) must not — a false "input needed"
    /// right before a fatal message reads as an instruction.
    fn replay(&self, for_input: bool) -> Vec<Change> {
        let mut out = vec![];
        if let Some(title) = &self.title {
            out.push(Change::Title(title.clone()));
        }
        if self.truncated {
            out.push(Change::Text("(earlier output truncated)\r\n".to_string()));
        }
        out.extend(self.changes.iter().cloned());
        if for_input {
            out.push(Change::Text(
                "\r\nThinkTerm needs your input to finish connecting.\r\n".to_string(),
            ));
        }
        out
    }
}

/// Where a materialized prompt should live.
struct HostPlacement {
    workspace: String,
    window_id: Option<crate::WindowId>,
}

/// Find an **on-screen** window that already hosts a pane of this domain.
///
/// Deliberately restricted to the active workspace, with no "any window that
/// has a pane of this domain" fallback: the caller is parked in `block_on`
/// waiting for the answer, so a prompt hosted in a background workspace can
/// never be answered and the domain never comes back. When there is no
/// on-screen window a standalone one (`window_id: None`) is strictly better —
/// it is visible immediately, and by the time we get here somebody really does
/// have to type something.
fn default_placement(domain_id: Option<DomainId>) -> Option<HostPlacement> {
    // A headless mux server has no main-thread scheduler; spawning there would
    // panic rather than answer.
    if !promise::spawn::is_scheduler_configured() {
        return None;
    }

    let (tx, rx) = crossbeam::channel::bounded(1);
    promise::spawn::spawn_into_main_thread(async move {
        let placement = Mux::try_get().map(|mux| {
            let workspace = mux.active_workspace();
            let window_id = domain_id.and_then(|domain_id| {
                mux.iter_windows_in_workspace(&workspace)
                    .into_iter()
                    .find(|window_id| {
                        mux.get_window(*window_id).map_or(false, |w| {
                            w.iter().any(|tab| {
                                tab.iter_panes_ignoring_zoom()
                                    .iter()
                                    .any(|p| p.pane.domain_id() == domain_id)
                            })
                        })
                    })
            });
            HostPlacement {
                workspace,
                window_id,
            }
        });
        tx.send(placement).ok();
    })
    .detach();

    // Bounded, because the main thread may be busy: a prompt in a fresh
    // standalone window beats no prompt at all. Blocking here is safe — this
    // is the relay thread, not the main thread, and whoever asked for input is
    // already parked waiting for this very prompt.
    rx.recv_timeout(Duration::from_secs(2)).ok().flatten()
}

/// Answer a request we are never going to service, so that its caller does not
/// park in `block_on` forever.
fn fail_request(req: UIRequest) {
    match req {
        UIRequest::Close | UIRequest::Output(_) | UIRequest::Materialize => {}
        UIRequest::Input { mut respond, .. } => {
            respond.result(Err(anyhow!("connection UI was closed")));
        }
        UIRequest::Sleep { mut respond, .. } => {
            respond.result(Err(anyhow!("connection UI was closed")));
        }
    }
}

/// The receiver-side relay behind [`ConnectionUI::new_lazy`].
///
/// `ConnectionUI` is `Clone` and its `tx` is copied into many callers, so the
/// backend cannot be swapped from the sending side. Instead this owns the
/// `Receiver` and decides, per request, whether the silent path will do.
struct LazyImpl {
    rx: Receiver<UIRequest>,
    params: ConnectionUIParams,
    alive: Arc<AtomicBool>,
    backlog: OutputBacklog,
    /// Present only once something forced a window into existence, paired with
    /// the workspace it was placed in.
    real: Option<(ConnectionUI, String)>,
    /// Requests pulled off `rx` while servicing a silent sleep, replayed in
    /// arrival order so nothing is reordered or lost.
    deferred: VecDeque<UIRequest>,
}

impl LazyImpl {
    fn run(&mut self) {
        loop {
            let req = match self.deferred.pop_front() {
                Some(req) => req,
                None => match self.rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(req) => req,
                    Err(err) if err.is_timeout() => continue,
                    // Every Sender is gone, so nobody can be waiting on us.
                    Err(_) => {
                        self.alive.store(false, Ordering::Release);
                        return;
                    }
                },
            };
            match req {
                UIRequest::Close => break,
                UIRequest::Output(changes) => self.on_output(changes),
                UIRequest::Materialize => {
                    self.ensure_materialized(false);
                }
                UIRequest::Input { .. } => self.on_input(req),
                UIRequest::Sleep { .. } => self.on_sleep(req),
            }
        }

        // Close the window, then answer — never drop — anything still queued or
        // still to arrive. We hold `rx` until the last Sender is gone because a
        // promise dropped instead of answered parks its caller in `block_on`
        // forever, and draining with `try_iter` then dropping `rx` would race a
        // sender that pushes in the gap.
        if let Some((ui, _)) = self.real.take() {
            ui.close();
        }
        self.alive.store(false, Ordering::Release);
        while let Some(req) = self.deferred.pop_front() {
            fail_request(req);
        }
        while let Ok(req) = self.rx.recv() {
            fail_request(req);
        }
    }

    fn on_output(&mut self, changes: Vec<Change>) {
        // Recorded even once visible, so that a rebuild after a Space switch
        // can replay the whole story rather than half of it.
        for change in &changes {
            self.backlog.push(change.clone());
        }
        if self.real.is_none() {
            return;
        }
        if self.forward(UIRequest::Output(changes)).is_some() {
            // The user closed the window. Go quiet again; the backlog still
            // holds the transcript for a later rebuild.
            log::info!("lazy connui: window closed; back to silent");
        }
    }

    fn on_sleep(&mut self, req: UIRequest) {
        if self.real.is_some() {
            // A window exists: let it draw the countdown and stay cancellable
            // exactly as it is today.
            self.forward_or_fail(req);
            return;
        }
        let UIRequest::Sleep {
            reason,
            duration,
            mut respond,
        } = req
        else {
            unreachable!("on_sleep called with {:?}", std::mem::discriminant(&req));
        };
        log::debug!("lazy connui: staying silent; {reason} (sleeping for {duration:?})");

        // Not `thread::sleep`: a `Close` from the reattach task runs
        // concurrently with this thread and must not sit behind up to the full
        // backoff. Anything that is not a Close is deferred, preserving order.
        let deadline = Instant::now() + duration;
        let mut closed = false;
        while let Some(remain) = deadline.checked_duration_since(Instant::now()) {
            match self.rx.recv_timeout(remain) {
                Ok(UIRequest::Close) => {
                    closed = true;
                    break;
                }
                Ok(other) => self.deferred.push_back(other),
                Err(err) if err.is_timeout() => break,
                // Every Sender is gone.
                Err(_) => {
                    closed = true;
                    break;
                }
            }
        }
        if closed {
            // Let the main loop take the shutdown path exactly once.
            self.deferred.push_back(UIRequest::Close);
            respond.result(Err(anyhow!("connection UI was closed")));
        } else {
            respond.result(Ok(()));
        }
    }

    fn on_input(&mut self, req: UIRequest) {
        self.ensure_materialized(true);
        if self.real.is_none() {
            // No Mux and no scheduler: reproduce HeadlessImpl's answer rather
            // than hang.
            match req {
                UIRequest::Input { mut respond, .. } => {
                    respond.result(Err(anyhow!("Input requested from headless context")));
                }
                other => fail_request(other),
            }
            return;
        }
        // Forward the request wholesale, promise included: ownership passes to
        // ConnectionUIImpl, which always answers it. The relay never creates a
        // second promise and never blocks on one.
        if let Some(req) = self.forward(req) {
            // The window vanished between materializing and sending. Rebuild
            // exactly once, then give up rather than loop.
            log::warn!("lazy connui: prompt window vanished; rebuilding once");
            self.ensure_materialized(true);
            if let Some(req) = self.forward(req) {
                fail_request(req);
            }
        }
    }

    /// Send to the materialized window. Returns the request back if there is no
    /// window, or if it just died (in which case we fall back to silent).
    fn forward(&mut self, req: UIRequest) -> Option<UIRequest> {
        let Some((ui, workspace)) = self.real.take() else {
            return Some(req);
        };
        // A successful send() only proves the channel exists. Once the
        // window's UI loop has returned (the user closed it; its close delay
        // may still be running) nothing services the queue, so gate on the
        // backend's liveness flag and treat a dying window as vanished — the
        // caller's rebuild-once path takes it from there.
        if !ui.test_alive() {
            return Some(req);
        }
        match ui.tx.send(req) {
            Ok(()) => {
                self.real = Some((ui, workspace));
                None
            }
            Err(err) => Some(err.into_inner()),
        }
    }

    fn forward_or_fail(&mut self, req: UIRequest) {
        if let Some(req) = self.forward(req) {
            fail_request(req);
        }
    }

    fn ensure_materialized(&mut self, for_input: bool) {
        let placement = default_placement(self.params.host_domain_id);

        // The user may have switched Spaces since the window was built. One in
        // a background workspace is off screen and its prompt could never be
        // answered, which would block the connection invisibly. Rebuild where
        // the user is now.
        if let (Some((_, hosted)), Some(p)) = (&self.real, &placement) {
            if !p.workspace.is_empty() && &p.workspace != hosted {
                log::info!(
                    "lazy connui: workspace changed {hosted:?} -> {:?}; rebuilding window",
                    p.workspace
                );
                if let Some((ui, _)) = self.real.take() {
                    ui.close();
                }
            }
        }
        if self.real.is_some() {
            return;
        }

        let (workspace, window_id) = match &placement {
            Some(p) => (p.workspace.clone(), p.window_id),
            None => {
                if !promise::spawn::is_scheduler_configured() {
                    // Headless: there is nothing to materialize into.
                    return;
                }
                (String::new(), None)
            }
        };

        log::warn!(
            "lazy connui: becoming visible in window {window_id:?} of workspace \
             {workspace:?}, replaying {} buffered changes",
            self.backlog.changes.len()
        );
        let ui = ConnectionUI::with_params(ConnectionUIParams {
            window_id,
            ..self.params
        });
        ui.output(self.backlog.replay(for_input));
        self.real = Some((ui, workspace));
    }
}

#[derive(Default, Clone, Copy, Debug)]
pub struct ConnectionUIParams {
    pub size: TerminalSize,
    pub disable_close_delay: bool,
    pub window_id: Option<crate::WindowId>,
    /// Only consulted by [`ConnectionUI::new_lazy`]. When a prompt finally
    /// forces a window into existence, prefer an on-screen window that already
    /// hosts a pane of this domain, so the question lands where the user is
    /// looking rather than in a window of its own over a frozen session.
    pub host_domain_id: Option<crate::domain::DomainId>,
}

#[derive(Clone)]
pub struct ConnectionUI {
    tx: Sender<UIRequest>,
    /// Cleared by the backend once it will no longer *service* requests.
    /// `tx.send()` alone cannot answer that for the lazy relay, which
    /// deliberately outlives its `Close`: it holds the `Receiver` until the
    /// last `Sender` drops so that queued promises get answered instead of
    /// dropped. A dropped `Promise` parks its caller in `block_on` forever —
    /// `promise` has no `Drop` impl that completes the future.
    alive: Arc<AtomicBool>,
}

impl ConnectionUI {
    pub fn new() -> Self {
        Self::with_params(Default::default())
    }

    pub fn with_params(params: ConnectionUIParams) -> Self {
        let (tx, rx) = unbounded();
        let alive = Arc::new(AtomicBool::new(true));
        let backend_alive = Arc::clone(&alive);
        promise::spawn::spawn_into_main_thread(termwiztermtab::run(
            params.size,
            params.window_id,
            move |term| {
                let mut ui = ConnectionUIImpl { term, rx };
                let status = ui.run().unwrap_or_else(|e| {
                    log::error!("while running ConnectionUI loop: {:?}", e);
                    CloseStatus::Implicit
                });

                if !params.disable_close_delay && status == CloseStatus::Implicit {
                    ui.sleep(
                        "(this window will close automatically)",
                        Duration::new(120, 0),
                    )
                    .ok();
                }
                // Cleared here rather than earlier so that `test_alive` keeps
                // reporting exactly what a live `rx` reported before this flag
                // existed: `get_error_window` reuses a window that is still
                // inside its close delay.
                backend_alive.store(false, Ordering::Release);
                // Anything still queued — sent while the close delay ran, or
                // racing the flag flip above — must be *failed*, never
                // dropped: a dropped promise parks its caller in block_on
                // forever. Senders that observe the cleared flag stop
                // sending, so this drain converges.
                let ConnectionUIImpl { rx, .. } = ui;
                while let Ok(req) = rx.try_recv() {
                    fail_request(req);
                }
                Ok(())
            },
            None,
        ))
        .detach();
        Self { tx, alive }
    }

    pub fn new_with_no_close_delay() -> Self {
        Self::with_params(ConnectionUIParams {
            disable_close_delay: true,
            ..Default::default()
        })
    }

    pub fn new_headless() -> Self {
        let (tx, rx) = unbounded();
        let alive = Arc::new(AtomicBool::new(true));
        let backend_alive = Arc::clone(&alive);
        std::thread::spawn(move || {
            let mut ui = HeadlessImpl { rx };
            let result = ui.run();
            backend_alive.store(false, Ordering::Release);
            result
        });
        Self { tx, alive }
    }

    /// A UI that costs nothing until it has something to ask.
    ///
    /// `Output` and `Sleep` are serviced silently — no window, no tab, no
    /// stolen focus. Only an `Input` request (an ssh password, a host-key
    /// confirmation, a keyboard-interactive prompt) materializes a real
    /// window, replays the output that led up to the question, and forwards
    /// the prompt to it.
    ///
    /// Silent is not invisible: a reconnecting mux domain already paints an
    /// opaque overlay across every pane, an orange sidebar Space icon, and a
    /// sidebar Reconnect row once it suspends. A progress window would only be
    /// a further copy of that, and one that steals the user's active tab.
    pub fn new_lazy(params: ConnectionUIParams) -> Self {
        let (tx, rx) = unbounded();
        let alive = Arc::new(AtomicBool::new(true));
        let backend_alive = Arc::clone(&alive);
        std::thread::Builder::new()
            .name("lazy-connui".into())
            .spawn(move || {
                LazyImpl {
                    rx,
                    params,
                    alive: backend_alive,
                    backlog: OutputBacklog::default(),
                    real: None,
                    deferred: VecDeque::new(),
                }
                .run()
            })
            .expect("failed to spawn lazy connui relay");
        Self { tx, alive }
    }

    pub fn run_and_log_error<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce() -> anyhow::Result<T>,
    {
        match f() {
            Err(e) => {
                let what = format!("\r\nFailed: {:?}\r\n", e);
                log::error!("{}", what);
                self.output_str(&what);
                Err(e)
            }
            result => result,
        }
    }

    pub async fn async_run_and_log_error<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: std::future::Future<Output = anyhow::Result<T>>,
    {
        match f.await {
            Err(e) => {
                let what = format!("\r\nFailed: {:?}\r\n", e);
                self.output_str(&what);
                Err(e)
            }
            result => result,
        }
    }

    pub fn title(&self, title: &str) {
        self.output(vec![Change::Title(title.to_string())]);
    }

    pub fn output(&self, changes: Vec<Change>) {
        self.tx.send(UIRequest::Output(changes)).ok();
    }

    pub fn output_str(&self, s: &str) {
        let s = s.replace("\n", "\r\n");
        self.output(vec![Change::Text(s)]);
    }

    /// Sleep (blocking!) for the specified duration, but updates
    /// the UI with the reason and a count down during that time.
    pub fn sleep_with_reason(&self, reason: &str, duration: Duration) -> anyhow::Result<()> {
        let mut promise = Promise::new();
        let future = promise.get_future().unwrap();

        self.tx
            .send(UIRequest::Sleep {
                reason: reason.to_string(),
                duration,
                respond: promise,
            })
            .context("send to ConnectionUI failed")?;

        block_on(future)
    }

    /// Crack a multi-line prompt into an optional preamble and the prompt
    /// text on the final line.  This is needed because the line editor
    /// is only designed for a single line prompt; a multi-line prompt
    /// messes up the cursor positioning.
    fn split_multi_line_prompt(s: &str) -> (Option<String>, String) {
        let text = s.replace("\n", "\r\n");
        let bits: Vec<&str> = text.rsplitn(2, "\r\n").collect();

        if bits.len() == 2 {
            (Some(format!("{}\r\n", bits[1])), bits[0].to_owned())
        } else {
            (None, text)
        }
    }

    pub fn input(&self, prompt: &str) -> anyhow::Result<String> {
        let mut promise = Promise::new();
        let future = promise.get_future().unwrap();

        let (preamble, prompt) = Self::split_multi_line_prompt(prompt);
        if let Some(preamble) = preamble {
            self.output(vec![Change::Text(preamble)]);
        }

        self.tx
            .send(UIRequest::Input {
                prompt,
                echo: true,
                respond: promise,
            })
            .context("send to ConnectionUI failed")?;

        block_on(future)
    }

    pub fn password(&self, prompt: &str) -> anyhow::Result<String> {
        let mut promise = Promise::new();
        let future = promise.get_future().unwrap();

        let (preamble, prompt) = Self::split_multi_line_prompt(prompt);
        if let Some(preamble) = preamble {
            self.output(vec![Change::Text(preamble)]);
        }

        self.tx
            .send(UIRequest::Input {
                prompt,
                echo: false,
                respond: promise,
            })
            .context("send to ConnectionUI failed")?;

        block_on(future)
    }

    pub fn close(&self) {
        self.tx.send(UIRequest::Close).ok();
    }

    /// Ask a lazily-materialized UI to become visible even though nothing is
    /// being read back, for the cases where the only thing we have to say is
    /// something the user must see (a changed host key, say). Every other
    /// backend ignores it.
    pub fn materialize(&self) {
        self.tx.send(UIRequest::Materialize).ok();
    }

    pub fn test_alive(&self) -> bool {
        if !self.alive.load(Ordering::Acquire) {
            return false;
        }
        if !self.tx.send(UIRequest::Output(vec![])).is_ok() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
        self.alive.load(Ordering::Acquire) && self.tx.send(UIRequest::Output(vec![])).is_ok()
    }
}

lazy_static::lazy_static! {
    static ref ERROR_WINDOW: Mutex<Option<ConnectionUI>> = Mutex::new(None);
}

fn get_error_window() -> ConnectionUI {
    let mut err = ERROR_WINDOW.lock().unwrap();
    if let Some(ui) = err.as_ref().map(|ui| ui.clone()) {
        ui.output_str("\n");
        if ui.test_alive() {
            return ui;
        }
    }

    let ui = ConnectionUI::new_with_no_close_delay();
    ui.title("wezterm Configuration Error");
    err.replace(ui.clone());
    ui
}

/// If the GUI has been started, pops up a window with the supplied error
/// message framed as a configuration error.
/// If there is no GUI front end, generates a toast notification instead.
pub fn show_configuration_error_message(err: &str) {
    log::error!("Configuration Error: {}", err);
    let ui = get_error_window();

    let mut wrapped = textwrap::fill(&err, 78);
    wrapped.push_str("\n");
    ui.output_str(&wrapped);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(changes: &[Change]) -> String {
        changes
            .iter()
            .filter_map(|c| match c {
                Change::Text(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn backlog_holds_the_title_out_of_the_transcript() {
        let mut backlog = OutputBacklog::default();
        backlog.push(Change::Title("ThinkTerm: Reconnecting...".to_string()));
        backlog.push(Change::Text("hello".to_string()));

        // The title must not sit in the ring, or trimming could drop it.
        assert_eq!(backlog.changes.len(), 1);
        assert_eq!(backlog.title.as_deref(), Some("ThinkTerm: Reconnecting..."));
        assert!(matches!(backlog.replay(true).first(), Some(Change::Title(_))));
    }

    #[test]
    fn backlog_drops_the_oldest_text_past_the_cap() {
        let mut backlog = OutputBacklog::default();
        for i in 0..(OutputBacklog::MAX_CHANGES + 10) {
            backlog.push(Change::Text(format!("line {i}\r\n")));
        }

        assert_eq!(backlog.changes.len(), OutputBacklog::MAX_CHANGES);
        assert!(backlog.truncated);

        let replayed = text_of(&backlog.replay(true));
        assert!(replayed.contains("(earlier output truncated)"));
        // The tail is what explains the prompt, so it is the front that goes.
        assert!(!replayed.contains("line 0\r\n"));
        assert!(replayed.contains(&format!("line {}\r\n", OutputBacklog::MAX_CHANGES + 9)));
    }

    #[test]
    fn backlog_replay_ends_by_saying_why_the_window_appeared() {
        let mut backlog = OutputBacklog::default();
        backlog.push(Change::Text("Connecting to host using SSH\r\n".to_string()));

        let replayed = text_of(&backlog.replay(true));
        assert!(replayed.starts_with("Connecting to host using SSH"));
        assert!(replayed.ends_with("ThinkTerm needs your input to finish connecting.\r\n"));

        // A window materialized only to show something (a fatal host-key
        // warning) must not falsely instruct the user to type.
        let informational = text_of(&backlog.replay(false));
        assert!(!informational.contains("needs your input"));
    }

    #[test]
    fn lazy_sleep_is_serviced_silently() {
        let ui = ConnectionUI::new_lazy(ConnectionUIParams::default());
        let start = Instant::now();

        // The silent path always succeeds: there is no window to close, so a
        // reconnect can never be "cancelled" out from under the user.
        assert!(ui
            .sleep_with_reason("under test", Duration::from_millis(50))
            .is_ok());
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn lazy_input_without_a_scheduler_answers_like_headless() {
        // No main-thread scheduler in a unit test, so nothing can be
        // materialized; the relay must answer rather than hang.
        let ui = ConnectionUI::new_lazy(ConnectionUIParams::default());
        let err = ui.input("password> ").unwrap_err();
        assert!(
            err.to_string().contains("headless"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn lazy_close_answers_a_later_input_instead_of_dropping_it() {
        let ui = ConnectionUI::new_lazy(ConnectionUIParams::default());
        ui.close();

        // The relay holds its Receiver until the last Sender drops precisely
        // so that this cannot park forever: a Promise dropped rather than
        // answered wedges its caller inside block_on with no way out.
        let (tx, rx) = crossbeam::channel::bounded(1);
        let probe = ui.clone();
        std::thread::spawn(move || {
            tx.send(probe.input("are you still there?")).ok();
        });

        let answered = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("input() must be answered after close, not dropped");
        assert!(answered.is_err());
    }

    #[test]
    fn lazy_test_alive_goes_false_once_closed() {
        let ui = ConnectionUI::new_lazy(ConnectionUIParams::default());
        assert!(ui.test_alive());

        ui.close();
        // `tx.send` keeps succeeding while the relay reaps, so only the
        // explicit liveness flag can answer this.
        std::thread::sleep(Duration::from_millis(300));
        assert!(!ui.test_alive());
    }
}
