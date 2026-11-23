//! Codex: its app-server daemon, which the TUI in the pane is itself a
//! client of. The bridge is one more client of the same daemon. It finds
//! the pane's thread among those loaded, joins it, and from then on sees
//! what the TUI sees and may do what the TUI may: say something (a turn
//! of its own, or added to the running one), stop the turn, answer an
//! approval -- the first answer from any client wins, and the daemon tells
//! the others it was given.
//!
//! The daemon listens on a Unix socket under `CODEX_HOME` and speaks
//! JSON-RPC (without the `jsonrpc` member) in WebSocket text messages.

use super::rollout::Rollout;
use super::{head, now, read_ops, tail, ws, Input, Out, Target};
use crate::{AgentState, Ask, AskKind, Attachment, Choice, CommandInfo, Ev, FileEdit, Item, Look, LookKind, ModelChoice, Need, NoteKind, Op, Status};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a call made while setting up may wait for its answer.
const CALL_WAIT: Duration = Duration::from_secs(30);
/// Turns of history sent at a time.
const TURNS_AT_ONCE: u32 = 6;
/// What is kept of a command's output and a file's diff.
const OUTPUT_KEPT: usize = 6 << 10;
const DIFF_KEPT: usize = 24 << 10;
/// Files of one change whose diff is kept.
const DIFFS_KEPT: usize = 12;
/// What is kept of what the agent said or thought, each, and of a command.
const TEXT_KEPT: usize = 256 << 10;
/// What is shown of a command asked about: its start, said to go on.
const ASK_KEPT: usize = 16 << 10;
/// The loaded threads looked at, at most: each costs a call.
const THREADS_LOOKED_AT: usize = 64;
/// What is shown of a skill's description.
const DESCRIPTION_KEPT: usize = 300;
/// How often what the agent is writing is sent as it grows.
const WRITING_EVERY: Duration = Duration::from_millis(150);
/// Codex's own commands the chat runs, as Codex words them: name, what it
/// does, what follows it.
const OWN_COMMANDS: [(&str, &str, Option<&str>); 3] = [
    ("/compact", "Summarize the conversation to free up context", None),
    ("/review", "Review the current changes and find issues", Some("[instructions]")),
    ("/rename", "Rename the conversation", Some("<name>")),
];
/// Codex's own commands that need its terminal: said so, rather than sent
/// to the model as words.
const TERMINAL_COMMANDS: [&str; 22] = [
    "new", "resume", "fork", "init", "diff", "mention", "status", "mcp", "apps", "skills", "approvals",
    "permissions", "personality", "plan", "collab", "agent", "logout", "quit", "exit", "feedback", "undo", "statusline",
];

/// Codex's folder: as the Codex in the pane has it, where that can be
/// read, else as this process's environment says.
pub fn codex_home(target: &Target) -> PathBuf {
    let set = super::pane_env(target, &["codex"], "CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from));
    match set.filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => dir,
        None => home().join(".codex"),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Where the managed daemon listens.
pub fn socket_path(target: &Target) -> PathBuf {
    codex_home(target).join("app-server-control").join("app-server-control.sock")
}

/// The daemon, as the bridge's loop talks to it.
struct Daemon {
    writer: Arc<Mutex<ws::Writer>>,
    rx: mpsc::Receiver<Input<Value>>,
    /// What came while a call waited for its answer, in order.
    later: VecDeque<Input<Value>>,
    next_id: u64,
}

impl Daemon {
    fn send(&self, message: &Value) -> Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        writer.text(&message.to_string()).context("writing to the Codex daemon")
    }

    /// Sends a request and goes on; its answer comes back through `next`.
    fn request(&mut self, method: &str, params: Value) -> Result<u64> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "id": id, "method": method, "params": params }))?;
        Ok(id)
    }

    /// Sends a request and waits for its answer: the result, or the
    /// daemon's error as the error.
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.request(method, params)?;
        let deadline = Instant::now() + CALL_WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let input = self
                .rx
                .recv_timeout(left)
                .map_err(|_| anyhow!("no answer from the Codex daemon to {method}"))?;
            match input {
                Input::Agent(message) if message.get("id").and_then(Value::as_u64) == Some(id) && message.get("method").is_none() => {
                    if let Some(error) = message.get("error") {
                        let text = error.get("message").and_then(Value::as_str).unwrap_or("error");
                        return Err(anyhow!("{method}: {text}"));
                    }
                    return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                }
                Input::AgentGone(why) => return Err(anyhow!("the Codex daemon went away: {why}")),
                other => self.later.push_back(other),
            }
        }
    }

    /// The next input; `Some(None)` when `wait` (if given) passes first,
    /// `None` once nothing more can come.
    fn next(&mut self, wait: Option<Duration>) -> Option<Option<Input<Value>>> {
        if let Some(input) = self.later.pop_front() {
            return Some(Some(input));
        }
        match wait {
            None => self.rx.recv().ok().map(Some),
            Some(wait) => match self.rx.recv_timeout(wait) {
                Ok(input) => Some(Some(input)),
                Err(mpsc::RecvTimeoutError::Timeout) => Some(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            },
        }
    }

    fn close(&self) {
        let mut writer = self.writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = writer.close();
    }
}

fn connect(target: &Target, input: impl BufRead + Send + 'static) -> Result<Daemon> {
    let path = socket_path(target);
    let (mut reader, writer) =
        ws::connect(&path).with_context(|| format!("connecting to the Codex daemon at {}", path.display()))?;
    let writer = Arc::new(Mutex::new(writer));
    let (tx, rx) = mpsc::channel();
    {
        let (tx, writer) = (tx.clone(), Arc::clone(&writer));
        std::thread::spawn(move || loop {
            match reader.next() {
                Ok(ws::Message::Text(text)) => match serde_json::from_str::<Value>(&text) {
                    Ok(message) => {
                        if tx.send(Input::Agent(message)).is_err() {
                            return;
                        }
                    }
                    Err(err) => log::warn!("agent chat: not JSON from the Codex daemon: {err}"),
                },
                Ok(ws::Message::Ping(payload)) => {
                    let mut writer = writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    let _ = writer.pong(&payload);
                }
                Ok(ws::Message::Close) => {
                    let _ = tx.send(Input::AgentGone("it closed the connection".into()));
                    return;
                }
                Err(err) => {
                    let _ = tx.send(Input::AgentGone(err.to_string()));
                    return;
                }
            }
        });
    }
    read_ops(input, tx);
    let mut daemon = Daemon {
        writer,
        rx,
        later: VecDeque::new(),
        next_id: 0,
    };
    daemon.call(
        "initialize",
        json!({
            "clientInfo": { "name": "thinkterm", "title": "ThinkTerm", "version": env!("CARGO_PKG_VERSION") },
            "capabilities": { "experimentalApi": true },
        }),
    )?;
    daemon.send(&json!({ "method": "initialized" }))?;
    Ok(daemon)
}

/// The pane's thread: a loaded one, not a sub-agent's, started where the
/// pane is. Among several the one working, else the one last active.
struct Found {
    id: String,
    name: Option<String>,
    status: Value,
    guessed: bool,
    /// The file Codex keeps of the thread.
    path: Option<PathBuf>,
}

fn find_thread(daemon: &mut Daemon, cwd: Option<&Path>) -> Result<Option<Found>> {
    let mut ids = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let listed = daemon.call("thread/loaded/list", json!({ "cursor": cursor }))?;
        ids.extend(
            listed["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|id| id.as_str().map(str::to_string)),
        );
        cursor = listed.get("nextCursor").cloned().unwrap_or(Value::Null);
        if cursor.is_null() || ids.len() >= THREADS_LOOKED_AT {
            break;
        }
    }
    let mut candidates = Vec::new();
    for id in ids.into_iter().take(THREADS_LOOKED_AT) {
        let Ok(read) = daemon.call("thread/read", json!({ "threadId": id })) else {
            continue;
        };
        let thread = &read["thread"];
        // A sub-agent's thread names its parent in its source.
        if thread["source"].is_object() || thread["ephemeral"].as_bool() == Some(true) {
            continue;
        }
        let here = match (cwd, thread["cwd"].as_str()) {
            (Some(cwd), Some(theirs)) => same_dir(cwd, Path::new(theirs)),
            (None, _) => true,
            (Some(_), None) => false,
        };
        if here {
            candidates.push(thread.clone());
        }
    }
    let working = |thread: &Value| thread["status"]["type"].as_str() == Some("active");
    let updated = |thread: &Value| thread["updatedAt"].as_i64().unwrap_or(0);
    candidates.sort_by(|a, b| working(b).cmp(&working(a)).then(updated(b).cmp(&updated(a))));
    // Not knowing where the pane is, any one found is a guess.
    let guessed = candidates.len() > 1 || cwd.is_none();
    Ok(candidates.into_iter().next().map(|thread| Found {
        id: thread["id"].as_str().unwrap_or_default().to_string(),
        name: thread["name"].as_str().map(str::to_string),
        status: thread["status"].clone(),
        guessed,
        path: thread["path"].as_str().map(PathBuf::from),
    }))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canonical(a) == canonical(b)
}

fn state_of(status: &Value) -> AgentState {
    match status["type"].as_str() {
        Some("active") => {
            let flags = status["activeFlags"].as_array();
            if flags.is_some_and(|flags| !flags.is_empty()) {
                AgentState::Blocked
            } else {
                AgentState::Working
            }
        }
        _ => AgentState::Idle,
    }
}

/// What the bridge holds of the joined thread.
struct Chat {
    thread: String,
    joined: bool,
    turn: Option<String>,
    state: AgentState,
    /// The cursor to the turns before those sent.
    older: Option<Value>,
    /// What is being asked: the ask's id, the request's own id (a number
    /// or a string), and its kind.
    asks: HashMap<String, (Value, Ask)>,
    /// Files of the changes seen, by item, to say which an approval is for.
    edits: HashMap<String, Vec<String>>,
    /// What each request in flight was for.
    pending: HashMap<u64, Pending>,
    /// The model the thread talks to, and how hard it thinks.
    model: Option<String>,
    effort: Option<String>,
    /// The models it could talk to.
    models: Vec<ModelChoice>,
    /// A model (and effort) asked for here: Codex takes it with the next
    /// turn started.
    next: Option<(String, Option<String>)>,
    /// History left out of the last page sent to fit it in a line: older
    /// than the page, and newer than what `older` points to, so sent first
    /// when more is asked for.
    held: Vec<Item>,
    /// The skills Codex has where the pane is, enabled, as `$name` takes
    /// them in.
    skills: Vec<Skill>,
    /// Where skills are looked for.
    cwd: Option<PathBuf>,
    /// What the agent is writing now, as far as it has come: its item,
    /// the words, when they were last sent, and whether more came since.
    writing: Option<Writing>,
    /// How full the context is: read from the thread's file, once Codex
    /// says where that is, and from what it reports as turns go on.
    rollout: Rollout,
}

struct Writing {
    id: String,
    text: String,
    sent: Instant,
    owed: bool,
}

/// A skill, as `skills/list` gives it.
#[derive(Clone, Debug, PartialEq)]
struct Skill {
    name: String,
    path: String,
    description: String,
}

impl Chat {
    /// More of what the agent writes: sent as it grows, a few times a
    /// second.
    fn wrote(&mut self, out: &Out, id: &str, delta: &str) -> Result<()> {
        match &mut self.writing {
            Some(writing) if writing.id == id => writing.text.push_str(delta),
            _ => {
                self.writing = Some(Writing {
                    id: id.to_string(),
                    text: delta.to_string(),
                    // The first words go at once.
                    sent: Instant::now().checked_sub(WRITING_EVERY).unwrap_or_else(Instant::now),
                    owed: false,
                })
            }
        }
        let due = self.writing.as_ref().is_some_and(|w| w.sent.elapsed() >= WRITING_EVERY);
        if due {
            self.send_writing(out)?;
        } else if let Some(writing) = &mut self.writing {
            writing.owed = true;
        }
        Ok(())
    }

    /// How long until what was written and not sent is due.
    fn writing_due(&self) -> Option<Duration> {
        self.writing
            .as_ref()
            .filter(|w| w.owed)
            .map(|w| WRITING_EVERY.saturating_sub(w.sent.elapsed()))
    }

    fn send_writing(&mut self, out: &Out) -> Result<()> {
        let Some(writing) = &mut self.writing else {
            return Ok(());
        };
        writing.sent = Instant::now();
        writing.owed = false;
        if writing.text.trim().is_empty() {
            return Ok(());
        }
        out.send(&Ev::Item {
            item: Item::Text {
                id: writing.id.clone(),
                text: head(&writing.text, TEXT_KEPT),
            },
        })?;
        Ok(())
    }

    /// How full the context is now, read on from the thread's file.
    fn send_context(&mut self, out: &Out) -> Result<()> {
        self.rollout.read_on();
        out.send(&Ev::Context {
            context: self.rollout.info(),
        })?;
        Ok(())
    }

    /// The commands the chat offers: Codex's own it runs, then the skills.
    fn commands(&self) -> Ev {
        let own = OWN_COMMANDS.iter().map(|(name, description, args)| CommandInfo {
            name: name.to_string(),
            description: description.to_string(),
            args: args.map(str::to_string),
        });
        let skills = self.skills.iter().map(|skill| CommandInfo {
            name: format!("${}", skill.name),
            description: skill.description.clone(),
            // What the skill is for follows it.
            args: Some(String::new()),
        });
        Ev::Commands {
            commands: own.chain(skills).collect(),
        }
    }

    fn list_skills(&mut self, daemon: &mut Daemon) -> Result<()> {
        let id = daemon.request("skills/list", self.skills_params())?;
        self.pending.insert(id, Pending::Skills);
        Ok(())
    }

    fn skills_params(&self) -> Value {
        match &self.cwd {
            Some(cwd) => json!({ "cwds": [cwd], "forceReload": true }),
            None => json!({ "forceReload": true }),
        }
    }

    fn models(&self) -> Ev {
        Ev::Models {
            current: self.model.clone(),
            effort: self.effort.clone(),
            models: self.models.clone(),
        }
    }

    /// A turn of its own for `input`, on the model asked for here if one
    /// was.
    fn start(&mut self, daemon: &mut Daemon, input: Value) -> Result<()> {
        let mut params = json!({ "threadId": self.thread, "input": input });
        let choice = self.next.take();
        if let Some((model, effort)) = &choice {
            params["model"] = json!(model);
            if let Some(effort) = effort {
                params["effort"] = json!(effort);
            }
        }
        let id = daemon.request("turn/start", params)?;
        self.pending.insert(id, Pending::Start(choice));
        Ok(())
    }

    /// A page of history as one line holds it, the rest held back for
    /// the next ask: `(items, more)`.
    fn page(&mut self, items: Vec<Item>) -> (Vec<Item>, bool) {
        let (held, page) = super::split_history(items);
        self.held = held;
        let more = !self.held.is_empty() || self.older.is_some();
        (page, more)
    }

    /// What is still asked, said again after history from the start, which
    /// a client takes to answer all asks of before.
    fn ask_again(&self, out: &Out) -> Result<()> {
        for (_, ask) in self.asks.values() {
            out.send(&Ev::Ask { ask: ask.clone() })?;
        }
        Ok(())
    }
}

enum Pending {
    /// A turn started, with the model asked for here it took, put back to
    /// go with the next one when this one is refused.
    Start(Option<(String, Option<String>)>),
    /// A steer, and what it said, sent again as a turn of its own when the
    /// turn it was for has ended meanwhile.
    Steer(Value),
    Interrupt,
    /// History before the cursor, which goes back for another try when
    /// the request fails.
    Older(Value),
    Join,
    Skills,
    /// One of Codex's own commands, run from the chat: refused or not.
    Command,
}

pub fn run(target: &Target, input: impl BufRead + Send + 'static, out: Out) -> Result<()> {
    let mut daemon = match connect(target, input) {
        Ok(daemon) => daemon,
        Err(err) => {
            out.error(Some(Need::CodexDaemon), format!("{err:#}"))?;
            return Ok(());
        }
    };
    let found = match find_thread(&mut daemon, target.cwd.as_deref()) {
        Ok(Some(found)) => found,
        Ok(None) => {
            out.error(
                Some(Need::CodexThread),
                format!(
                    "no Codex conversation loaded in {}",
                    target.cwd.as_deref().map_or("any folder".into(), |cwd| cwd.display().to_string())
                ),
            )?;
            daemon.close();
            return Ok(());
        }
        Err(err) => {
            out.error(Some(Need::CodexDaemon), format!("{err:#}"))?;
            daemon.close();
            return Ok(());
        }
    };
    out.send(&Ev::Hello {
        protocol: crate::PROTOCOL,
        agent: "codex".into(),
        title: found.name.clone(),
        guessed: found.guessed,
    })?;
    let mut chat = Chat {
        thread: found.id.clone(),
        joined: false,
        turn: None,
        state: state_of(&found.status),
        older: None,
        asks: HashMap::new(),
        edits: HashMap::new(),
        pending: HashMap::new(),
        model: None,
        effort: None,
        models: Vec::new(),
        next: None,
        held: Vec::new(),
        skills: Vec::new(),
        cwd: target.cwd.clone(),
        writing: None,
        rollout: Rollout::new(found.path.clone()),
    };
    // A thread whose first turn has not run has nothing on disk yet and
    // cannot be joined; it is joined as soon as it starts.
    match daemon.call("thread/resume", json!({ "threadId": chat.thread, "excludeTurns": true })) {
        Ok(resumed) => {
            chat.joined = true;
            settings_of(&mut chat, &resumed);
        }
        Err(err) => log::info!("agent chat: not joined yet: {err:#}"),
    }
    match daemon.call("model/list", json!({})) {
        Ok(listed) => chat.models = models_of(&listed),
        Err(err) => log::info!("agent chat: no models: {err:#}"),
    }
    match daemon.call("skills/list", chat.skills_params()) {
        Ok(listed) => chat.skills = skills_of(&listed),
        Err(err) => log::info!("agent chat: no skills: {err:#}"),
    }
    let turns = if chat.joined {
        history(&mut daemon, &chat.thread, None)?
    } else {
        Turns {
            items: Vec::new(),
            older: None,
            running: None,
        }
    };
    chat.older = turns.older;
    chat.turn = turns.running;
    let (items, more) = chat.page(turns.items);
    out.send(&Ev::History { items, more, reset: true })?;
    out.send(&Ev::State {
        state: chat.state,
        since: None,
    })?;
    out.send(&chat.models())?;
    out.send(&chat.commands())?;
    chat.send_context(&out)?;

    while let Some(input) = daemon.next(chat.writing_due()) {
        let Some(input) = input else {
            chat.send_writing(&out)?;
            continue;
        };
        let going = match input {
            Input::ClientGone => false,
            Input::AgentGone(why) => {
                out.error(Some(Need::CodexDaemon), format!("the Codex daemon went away: {why}"))?;
                false
            }
            Input::Op(op) => {
                handle_op(&mut daemon, &mut chat, &out, op)?;
                true
            }
            Input::Agent(message) => handle_message(&mut daemon, &mut chat, &out, message)?,
        };
        if !going {
            break;
        }
    }
    daemon.close();
    Ok(())
}

/// Turns before `cursor` (the latest without one): as items oldest first,
/// the cursor to those before them, and the turn still running, if one is.
fn history(daemon: &mut Daemon, thread: &str, cursor: Option<Value>) -> Result<Turns> {
    let listed = daemon.call(
        "thread/turns/list",
        json!({
            "threadId": thread,
            "limit": TURNS_AT_ONCE,
            "sortDirection": "desc",
            "itemsView": "full",
            "cursor": cursor.unwrap_or(Value::Null),
        }),
    )?;
    Ok(turns_to_items(&listed))
}

struct Turns {
    items: Vec<Item>,
    older: Option<Value>,
    running: Option<String>,
}

fn turns_to_items(listed: &Value) -> Turns {
    let mut turns: Vec<&Value> = listed["data"].as_array().into_iter().flatten().collect();
    // Newest first as listed: the first may be the one running.
    let running = turns
        .first()
        .filter(|turn| turn["status"].as_str() == Some("inProgress"))
        .and_then(|turn| turn["id"].as_str())
        .map(str::to_string);
    turns.reverse();
    let mut items = Vec::new();
    for turn in turns {
        for item in turn["items"].as_array().into_iter().flatten() {
            if let Some(item) = to_item(item) {
                items.push(item);
            }
        }
        if let Some(note) = turn_note(turn) {
            items.push(note);
        }
    }
    let older = listed.get("nextCursor").filter(|c| !c.is_null()).cloned();
    Turns { items, older, running }
}

/// How a turn ended, when it did not simply end.
fn turn_note(turn: &Value) -> Option<Item> {
    let id = format!("{}-end", turn["id"].as_str()?);
    match turn["status"].as_str()? {
        "interrupted" => Some(Item::Note {
            id,
            note: NoteKind::Interrupted,
            text: String::new(),
        }),
        "failed" => Some(Item::Note {
            id,
            note: NoteKind::Failed,
            text: turn["error"]["message"].as_str().unwrap_or_default().to_string(),
        }),
        _ => None,
    }
}

fn handle_op(daemon: &mut Daemon, chat: &mut Chat, out: &Out, op: Op) -> Result<()> {
    match op {
        Op::Send { text, attachments } => {
            if text.trim().is_empty() && attachments.is_empty() {
                return Ok(());
            }
            if attachments.is_empty() && own_command(daemon, chat, out, &text)? {
                return Ok(());
            }
            let input = user_input(&text, &attachments, &chat.skills);
            match chat.turn.clone() {
                Some(turn) => {
                    let id = daemon.request(
                        "turn/steer",
                        json!({ "threadId": chat.thread, "expectedTurnId": turn, "input": input.clone() }),
                    )?;
                    chat.pending.insert(id, Pending::Steer(input));
                }
                None => chat.start(daemon, input)?,
            }
        }
        Op::SetModel { model, effort } => {
            if !chat.models.is_empty() && !chat.models.iter().any(|known| known.id == model) {
                out.refused(format!("Codex has no model {model}"))?;
                return Ok(());
            }
            chat.next = Some((model.clone(), effort.clone()));
            chat.model = Some(model);
            chat.effort = effort;
            out.send(&chat.models())?;
        }
        Op::Interrupt => match chat.turn.clone() {
            Some(turn) => {
                let id = daemon.request("turn/interrupt", json!({ "threadId": chat.thread, "turnId": turn }))?;
                chat.pending.insert(id, Pending::Interrupt);
            }
            None => out.refused("nothing is running")?,
        },
        Op::Answer { ask, choice } => match chat.asks.remove(&ask) {
            Some((request, _)) => {
                let decision = match choice {
                    Choice::Accept => "accept",
                    Choice::AcceptForSession => "acceptForSession",
                    Choice::Decline => "decline",
                };
                daemon.send(&json!({ "id": request, "result": { "decision": decision } }))?;
                out.send(&Ev::Resolved { id: ask })?;
            }
            None => out.refused("that question is no longer asked")?,
        },
        // What the last page left out goes first: it is newer than the
        // turns before it.
        Op::Older if !chat.held.is_empty() => {
            let held = std::mem::take(&mut chat.held);
            let (items, more) = chat.page(held);
            out.send(&Ev::History { items, more, reset: false })?;
        }
        Op::Older => match chat.older.take() {
            Some(cursor) => {
                // Asked and answered in the loop: history is long to read.
                let id = daemon.request(
                    "thread/turns/list",
                    json!({
                        "threadId": chat.thread,
                        "limit": TURNS_AT_ONCE,
                        "sortDirection": "desc",
                        "itemsView": "full",
                        "cursor": cursor.clone(),
                    }),
                )?;
                chat.pending.insert(id, Pending::Older(cursor));
            }
            None => out.send(&Ev::History {
                items: Vec::new(),
                more: false,
                reset: false,
            })?,
        },
        Op::Install => out.refused("Codex needs nothing installed")?,
        Op::Reload => out.refused("Codex has no extension to load")?,
    }
    Ok(())
}

/// Returns whether the chat goes on.
fn handle_message(daemon: &mut Daemon, chat: &mut Chat, out: &Out, message: Value) -> Result<bool> {
    let method = message.get("method").and_then(Value::as_str);
    let id = message.get("id").cloned();
    match (method, id) {
        // The answer to a request made from the loop.
        (None, Some(id)) => {
            let Some(pending) = id.as_u64().and_then(|id| chat.pending.remove(&id)) else {
                return Ok(true);
            };
            let error = message
                .get("error")
                .map(|e| e["message"].as_str().unwrap_or("refused").to_string());
            match (pending, error) {
                (Pending::Steer(input), Some(_)) => {
                    // The turn ended before the words reached it: they
                    // start a turn of their own.
                    chat.start(daemon, input)?;
                }
                (Pending::Older(_), None) => {
                    let turns = turns_to_items(&message["result"]);
                    chat.older = turns.older;
                    let (items, more) = chat.page(turns.items);
                    out.send(&Ev::History { items, more, reset: false })?;
                }
                (Pending::Older(cursor), Some(err)) => {
                    chat.older = Some(cursor);
                    out.refused(err)?;
                }
                (Pending::Join, None) => {
                    chat.joined = true;
                    if let Some(path) = message["result"]["thread"]["path"].as_str() {
                        chat.rollout.found(PathBuf::from(path));
                    }
                    settings_of(chat, &message["result"]);
                    out.send(&chat.models())?;
                    let turns = history(daemon, &chat.thread, None)?;
                    chat.older = turns.older;
                    if chat.turn.is_none() {
                        chat.turn = turns.running;
                    }
                    let (items, more) = chat.page(turns.items);
                    out.send(&Ev::History { items, more, reset: true })?;
                    chat.ask_again(out)?;
                }
                (Pending::Join, Some(err)) => log::info!("agent chat: joining failed: {err}"),
                (Pending::Skills, None) => {
                    let skills = skills_of(&message["result"]);
                    if skills != chat.skills {
                        chat.skills = skills;
                        out.send(&chat.commands())?;
                    }
                }
                (Pending::Skills, Some(err)) => log::info!("agent chat: no skills: {err}"),
                (Pending::Start(choice), Some(err)) => {
                    // The model asked for goes with the next turn instead,
                    // unless another was asked for meanwhile.
                    if chat.next.is_none() {
                        chat.next = choice;
                    }
                    out.refused(err)?;
                }
                (_, Some(err)) => out.refused(err)?,
                (_, None) => {}
            }
            Ok(true)
        }
        // A request of the daemon's: an approval this chat may answer.
        (Some(method), Some(request)) => {
            let params = &message["params"];
            if params["threadId"].as_str() != Some(chat.thread.as_str()) {
                return Ok(true);
            }
            let ask = match method {
                "item/commandExecution/requestApproval" => Some(Ask {
                    id: ask_id(&request),
                    kind: AskKind::Command,
                    body: cut(&display_command(params["command"].as_str().unwrap_or_default()), ASK_KEPT),
                    reason: params["reason"].as_str().map(str::to_string),
                    cwd: params["cwd"].as_str().map(str::to_string),
                    choices: vec![Choice::Accept, Choice::AcceptForSession, Choice::Decline],
                }),
                "item/fileChange/requestApproval" => {
                    let item = params["itemId"].as_str().unwrap_or_default();
                    let files = chat.edits.get(item).cloned().unwrap_or_default();
                    Some(Ask {
                        id: ask_id(&request),
                        kind: AskKind::Edit,
                        body: files.join("\n"),
                        reason: params["reason"].as_str().map(str::to_string),
                        cwd: None,
                        choices: vec![Choice::Accept, Choice::AcceptForSession, Choice::Decline],
                    })
                }
                // Other questions (input, permissions, MCP) are answered in
                // the terminal.
                _ => None,
            };
            if let Some(ask) = ask {
                chat.asks.insert(ask.id.clone(), (request, ask.clone()));
                out.send(&Ev::Ask { ask })?;
            }
            Ok(true)
        }
        // Skills changed on disk: of every thread, so said without one.
        (Some("skills/changed"), None) => {
            chat.list_skills(daemon)?;
            Ok(true)
        }
        (Some(method), None) => {
            let params = &message["params"];
            if params["threadId"].as_str() != Some(chat.thread.as_str()) {
                return Ok(true);
            }
            match method {
                "item/agentMessage/delta" => {
                    if let (Some(id), Some(delta)) = (params["itemId"].as_str(), params["delta"].as_str()) {
                        chat.wrote(out, id, delta)?;
                    }
                }
                "item/started" | "item/completed" => {
                    let item = &params["item"];
                    // Written whole: the words as they came are done with.
                    if method == "item/completed" && chat.writing.as_ref().is_some_and(|w| Some(w.id.as_str()) == item["id"].as_str()) {
                        chat.writing = None;
                    }
                    if item["type"].as_str() == Some("fileChange") {
                        if let Some(id) = item["id"].as_str() {
                            let files = item["changes"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|change| change["path"].as_str().map(str::to_string))
                                .collect();
                            chat.edits.insert(id.to_string(), files);
                        }
                    }
                    if let Some(item) = to_item(item) {
                        out.send(&Ev::Item { item })?;
                    }
                }
                "turn/started" => {
                    chat.turn = params["turn"]["id"].as_str().map(str::to_string);
                    set_state(chat, out, AgentState::Working)?;
                }
                // What a turn's model call took of the context.
                // The file first: what it holds may be older than this.
                "thread/tokenUsage/updated" => {
                    chat.rollout.read_on();
                    chat.rollout.reported(&params["tokenUsage"]);
                    out.send(&Ev::Context {
                        context: chat.rollout.info(),
                    })?;
                }
                "turn/completed" => {
                    chat.turn = None;
                    chat.send_context(out)?;
                    if let Some(note) = turn_note(&params["turn"]) {
                        out.send(&Ev::Item { item: note })?;
                    }
                }
                "thread/status/changed" => {
                    let state = state_of(&params["status"]);
                    set_state(chat, out, state)?;
                    if !chat.joined && state != AgentState::Idle {
                        let id = daemon.request("thread/resume", json!({ "threadId": chat.thread, "excludeTurns": true }))?;
                        chat.pending.insert(id, Pending::Join);
                    }
                }
                "serverRequest/resolved" => {
                    let id = ask_id(&params["requestId"]);
                    if chat.asks.remove(&id).is_some() {
                        out.send(&Ev::Resolved { id })?;
                    }
                }
                "thread/settings/updated" => {
                    settings_of(chat, &params["threadSettings"]);
                    out.send(&chat.models())?;
                }
                "thread/name/updated" => {
                    out.send(&Ev::Hello {
                        protocol: crate::PROTOCOL,
                        agent: "codex".into(),
                        title: params["threadName"].as_str().map(str::to_string),
                        guessed: false,
                    })?;
                }
                "thread/closed" => {
                    out.error(Some(Need::CodexThread), "the conversation was closed")?;
                    return Ok(false);
                }
                _ => {}
            }
            Ok(true)
        }
        (None, None) => Ok(true),
    }
}

fn set_state(chat: &mut Chat, out: &Out, state: AgentState) -> Result<()> {
    if chat.state != state {
        chat.state = state;
        out.send(&Ev::State {
            state,
            since: Some(now()),
        })?;
    }
    Ok(())
}

/// A request id as the client sees it.
fn ask_id(request: &Value) -> String {
    match request {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A Codex thread item as a chat item; `None` for what a chat leaves out.
fn to_item(item: &Value) -> Option<Item> {
    let id = item["id"].as_str()?.to_string();
    Some(match item["type"].as_str()? {
        "userMessage" => {
            let mut said = Vec::new();
            let mut attachments = Vec::new();
            for part in item["content"].as_array().into_iter().flatten() {
                match part["type"].as_str() {
                    Some("text") => said.extend(part["text"].as_str().map(str::to_string)),
                    Some("localImage") => {
                        let path = part["path"].as_str().unwrap_or_default();
                        attachments.push(Attachment {
                            name: path.rsplit('/').next().unwrap_or(path).to_string(),
                            image: true,
                            path: (!path.is_empty()).then(|| path.to_string()),
                            thumb: super::thumb::of_file(path),
                        });
                    }
                    Some("image") => {
                        let url = part["url"].as_str().or(part["image_url"].as_str()).unwrap_or_default();
                        attachments.push(Attachment {
                            name: "image".into(),
                            image: true,
                            path: None,
                            thumb: super::thumb::of_data_url(url),
                        });
                    }
                    _ => {}
                }
            }
            Item::User {
                id,
                text: said.join("\n"),
                attachments,
            }
        }
        "agentMessage" => {
            let text = item["text"].as_str().unwrap_or_default();
            if text.trim().is_empty() {
                return None;
            }
            Item::Text {
                id,
                text: head(text, TEXT_KEPT),
            }
        }
        "plan" => Item::Text {
            id,
            text: item["text"].as_str().unwrap_or_default().to_string(),
        },
        "reasoning" => {
            let parts = |key: &str| -> Vec<String> {
                item[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part.as_str().map(str::to_string))
                    .filter(|part| !part.trim().is_empty())
                    .collect()
            };
            let mut text = parts("summary");
            if text.is_empty() {
                text = parts("content");
            }
            if text.is_empty() {
                return None;
            }
            Item::Thinking {
                id,
                text: head(&text.join("\n\n"), TEXT_KEPT),
            }
        }
        "commandExecution" => {
            let exit = item["exitCode"].as_i64().map(|code| code as i32);
            let status = match item["status"].as_str() {
                Some("inProgress") => Status::Running,
                Some("declined") => Status::Declined,
                Some("failed") => Status::Failed,
                _ if exit.is_some_and(|code| code != 0) => Status::Failed,
                _ => Status::Done,
            };
            let looks = item["commandActions"]
                .as_array()
                .map(|actions| looks_of(actions))
                .unwrap_or_default();
            Item::Command {
                id,
                command: head(&display_command(item["command"].as_str().unwrap_or_default()), TEXT_KEPT),
                status,
                output: tail(item["aggregatedOutput"].as_str().unwrap_or_default(), OUTPUT_KEPT),
                exit,
                looks,
            }
        }
        "fileChange" => {
            let files = item["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(|(n, change)| {
                    let diff = change["diff"].as_str().unwrap_or_default();
                    let (added, removed) = crate::diff::count(diff);
                    FileEdit {
                        path: change["path"].as_str().unwrap_or_default().to_string(),
                        added,
                        removed,
                        // A change of many files shows the diffs of the
                        // first few; the rest are counted.
                        diff: if n < DIFFS_KEPT { head(diff, DIFF_KEPT) } else { String::new() },
                    }
                })
                .collect();
            Item::Edit {
                id,
                status: status_of(item["status"].as_str()),
                files,
            }
        }
        "mcpToolCall" => Item::Tool {
            id,
            name: format!(
                "{}.{}",
                item["server"].as_str().unwrap_or_default(),
                item["tool"].as_str().unwrap_or_default()
            ),
            detail: String::new(),
            status: status_of(item["status"].as_str()),
        },
        "dynamicToolCall" => Item::Tool {
            id,
            name: item["tool"].as_str().unwrap_or_default().to_string(),
            detail: String::new(),
            status: status_of(item["status"].as_str()),
        },
        "webSearch" => Item::Tool {
            id,
            name: "web_search".into(),
            detail: item["query"].as_str().unwrap_or_default().to_string(),
            status: Status::Done,
        },
        "imageView" => Item::Tool {
            id,
            name: "view_image".into(),
            detail: item["path"].as_str().unwrap_or_default().to_string(),
            status: Status::Done,
        },
        "collabAgentToolCall" | "subAgentActivity" => Item::Tool {
            id,
            name: "agent".into(),
            detail: item["agentPath"]
                .as_str()
                .or_else(|| item["prompt"].as_str())
                .map(|text| head(text, 200))
                .unwrap_or_default(),
            status: status_of(item["status"].as_str()),
        },
        "contextCompaction" => Item::Note {
            id,
            note: NoteKind::Compacted,
            text: String::new(),
        },
        // What a review found, said as the agent's answer.
        "exitedReviewMode" => {
            let text = item["review"].as_str().unwrap_or_default();
            if text.trim().is_empty() {
                return None;
            }
            Item::Text {
                id,
                text: tail(text, TEXT_KEPT),
            }
        }
        _ => return None,
    })
}

fn status_of(status: Option<&str>) -> Status {
    match status {
        Some("inProgress") | Some("running") | Some("pending") => Status::Running,
        Some("failed") | Some("error") => Status::Failed,
        Some("declined") => Status::Declined,
        _ => Status::Done,
    }
}

fn looks_of(actions: &[Value]) -> Vec<Look> {
    let mut looks = Vec::new();
    for action in actions {
        let look = match action["type"].as_str() {
            Some("read") => Look {
                kind: LookKind::Read,
                target: action["name"]
                    .as_str()
                    .or_else(|| action["path"].as_str())
                    .unwrap_or_default()
                    .to_string(),
            },
            Some("search") => Look {
                kind: LookKind::Search,
                target: action["query"]
                    .as_str()
                    .or_else(|| action["path"].as_str())
                    .unwrap_or_default()
                    .to_string(),
            },
            Some("listFiles") => Look {
                kind: LookKind::List,
                target: action["path"].as_str().unwrap_or(".").to_string(),
            },
            // A command that does more than look is shown as itself.
            _ => return Vec::new(),
        };
        looks.push(look);
    }
    looks
}

/// A command of Codex's own, sent as words: run as the terminal would, or
/// said to need the terminal. False for words to send on.
fn own_command(daemon: &mut Daemon, chat: &mut Chat, out: &Out, text: &str) -> Result<bool> {
    let text = text.trim();
    let Some(rest) = text.strip_prefix('/') else {
        return Ok(false);
    };
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let args = args.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':') {
        return Ok(false);
    }
    let idle = |out: &Out| -> Result<bool> {
        if chat.turn.is_none() {
            return Ok(true);
        }
        out.refused(format!("/{name} waits until Codex is done (or stopped)"))?;
        Ok(false)
    };
    let (method, params) = match name {
        "compact" => {
            if !idle(out)? {
                return Ok(true);
            }
            ("thread/compact/start", json!({ "threadId": chat.thread }))
        }
        "review" => {
            if !idle(out)? {
                return Ok(true);
            }
            let target = if args.is_empty() {
                json!({ "type": "uncommittedChanges" })
            } else {
                json!({ "type": "custom", "instructions": args })
            };
            ("review/start", json!({ "threadId": chat.thread, "target": target }))
        }
        "rename" => {
            if args.is_empty() {
                out.refused("/rename needs a name")?;
                return Ok(true);
            }
            ("thread/name/set", json!({ "threadId": chat.thread, "name": args }))
        }
        "model" => {
            if args.is_empty() {
                out.refused("/model takes a model's id: pick one from /model in the list")?;
            } else {
                handle_op(
                    daemon,
                    chat,
                    out,
                    Op::SetModel {
                        model: args.to_string(),
                        effort: None,
                    },
                )?;
            }
            return Ok(true);
        }
        _ if TERMINAL_COMMANDS.contains(&name) => {
            out.refused(format!("/{name} works in the terminal only"))?;
            return Ok(true);
        }
        _ => return Ok(false),
    };
    let id = daemon.request(method, params)?;
    chat.pending.insert(id, Pending::Command);
    Ok(true)
}

/// The enabled skills `skills/list` gives, of every folder it was asked
/// about, each once.
fn skills_of(listed: &Value) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    for entry in listed["data"].as_array().into_iter().flatten() {
        for skill in entry["skills"].as_array().into_iter().flatten() {
            if skill["enabled"].as_bool() == Some(false) {
                continue;
            }
            let (Some(name), Some(path)) = (skill["name"].as_str(), skill["path"].as_str()) else {
                continue;
            };
            if skills.iter().any(|known| known.name == name) {
                continue;
            }
            let description = skill["interface"]["shortDescription"]
                .as_str()
                .or(skill["shortDescription"].as_str())
                .or(skill["description"].as_str())
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            skills.push(Skill {
                name: name.to_string(),
                path: path.to_string(),
                description: head(&description, DESCRIPTION_KEPT),
            });
        }
    }
    skills
}

/// What the person said, as Codex takes it in: the words, the paths of
/// the files sent with them, the pictures as pictures, and each skill
/// named (`$name`) as that skill.
fn user_input(text: &str, attachments: &[Attachment], skills: &[Skill]) -> Value {
    let mut words = text.to_string();
    for file in attachments.iter().filter(|file| !file.image) {
        if let Some(path) = &file.path {
            if !words.is_empty() {
                words.push('\n');
            }
            words.push_str(path);
        }
    }
    let mut input = vec![json!({ "type": "text", "text": words, "text_elements": [] })];
    for picture in attachments.iter().filter(|file| file.image) {
        if let Some(path) = &picture.path {
            input.push(json!({ "type": "localImage", "path": path }));
        }
    }
    for skill in skills.iter().filter(|skill| names_skill(text, &skill.name)) {
        input.push(json!({ "type": "skill", "name": skill.name, "path": skill.path }));
    }
    Value::Array(input)
}

/// Whether `text` names the skill as a word of its own: `$name`, not
/// followed by more of a name.
fn names_skill(text: &str, name: &str) -> bool {
    let mention = format!("${name}");
    text.match_indices(&mention).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + mention.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
            && !after.is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == ':')
    })
}

/// The model a thread talks to and how hard it thinks, as Codex reports
/// them: on joining (`model`, `reasoningEffort`) and on a change
/// (`threadSettings`: `model`, `effort`).
fn settings_of(chat: &mut Chat, settings: &Value) {
    if let Some(model) = settings["model"].as_str().filter(|m| !m.is_empty()) {
        chat.model = Some(model.to_string());
        chat.effort = settings["reasoningEffort"]
            .as_str()
            .or(settings["effort"].as_str())
            .map(str::to_string);
    }
}

/// The models `model/list` offers, those hidden from its own picker
/// left out.
fn models_of(listed: &Value) -> Vec<ModelChoice> {
    listed["data"]
        .as_array()
        .or(listed["models"].as_array())
        .into_iter()
        .flatten()
        .filter(|model| !model["hidden"].as_bool().unwrap_or(false))
        .filter_map(|model| {
            let id = model["model"].as_str().or(model["id"].as_str())?.to_string();
            Some(ModelChoice {
                name: model["displayName"].as_str().unwrap_or(&id).to_string(),
                efforts: model["supportedReasoningEfforts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|option| option["reasoningEffort"].as_str().map(str::to_string))
                    .collect(),
                id,
            })
        })
        .collect()
}

/// `bash -lc 'cargo test'` as `cargo test`: the shell Codex wraps each
/// command in says nothing.
fn display_command(command: &str) -> String {
    let command = command.trim();
    for shell in ["/bin/bash", "/bin/zsh", "/bin/sh", "bash", "zsh", "sh", "/usr/bin/bash", "/usr/bin/zsh"] {
        for flags in [" -lc ", " -c "] {
            if let Some(rest) = command.strip_prefix(&format!("{shell}{flags}")) {
                let rest = rest.trim();
                if rest.len() >= 2 && rest.starts_with('\'') && rest.ends_with('\'') && !rest[1..rest.len() - 1].contains('\'') {
                    return rest[1..rest.len() - 1].to_string();
                }
                if rest.len() >= 2 && rest.starts_with('"') && rest.ends_with('"') && one_word(&rest[1..rest.len() - 1]) {
                    return unquote_double(&rest[1..rest.len() - 1]);
                }
                return rest.to_string();
            }
        }
    }
    command.to_string()
}

/// The inside of a shell's double quotes, as the shell reads it: a
/// backslash keeps only `\`, `"`, `$` and `` ` `` from their meaning.
fn unquote_double(quoted: &str) -> String {
    let mut out = String::with_capacity(quoted.len());
    let mut chars = quoted.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                if matches!(next, '\\' | '"' | '$' | '`') {
                    out.push(next);
                    chars.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

/// Whether the inside of double quotes is all one quoted word: no quote
/// in it ends the quoting (`"a" && "b"` is two), nor is the last quote
/// taken from its meaning by a backslash.
fn one_word(quoted: &str) -> bool {
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if chars.next().is_none() {
                    return false;
                }
            }
            '"' => return false,
            _ => {}
        }
    }
    true
}

/// The start of `text`, and a word that it goes on when cut.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    format!("{}\n…", head(text, max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_go_by_path_and_pictures_as_pictures() {
        let input = user_input(
            "look",
            &[
                Attachment {
                    name: "a.pdf".into(),
                    image: false,
                    path: Some("/u/ThinkTerm_Uploads/a.pdf".into()),
                    thumb: None,
                },
                Attachment {
                    name: "b.png".into(),
                    image: true,
                    path: Some("/u/ThinkTerm_Uploads/b.png".into()),
                    thumb: None,
                },
            ],
            &[],
        );
        assert_eq!(
            input,
            json!([
                { "type": "text", "text": "look\n/u/ThinkTerm_Uploads/a.pdf", "text_elements": [] },
                { "type": "localImage", "path": "/u/ThinkTerm_Uploads/b.png" }
            ])
        );
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Sink {
        fn texts(&self) -> Vec<String> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .filter_map(|line| match serde_json::from_str::<Ev>(line).unwrap() {
                    Ev::Item { item: Item::Text { text, .. } } => Some(text),
                    _ => None,
                })
                .collect()
        }
    }

    #[test]
    fn what_is_written_goes_as_it_grows_a_few_times_a_second() {
        let sink = Sink::default();
        let out = Out::new(Box::new(sink.clone()));
        let mut chat = Chat {
            thread: "t".into(),
            joined: true,
            turn: None,
            state: AgentState::Working,
            older: None,
            asks: HashMap::new(),
            edits: HashMap::new(),
            pending: HashMap::new(),
            model: None,
            effort: None,
            models: Vec::new(),
            next: None,
            held: Vec::new(),
            skills: Vec::new(),
            cwd: None,
            writing: None,
            rollout: Rollout::new(None),
        };
        // The first words go at once; those right after wait their turn.
        chat.wrote(&out, "m1", "Hello").unwrap();
        chat.wrote(&out, "m1", ", world").unwrap();
        assert_eq!(sink.texts(), ["Hello"]);
        let due = chat.writing_due().expect("more is owed");
        assert!(due <= WRITING_EVERY);
        chat.send_writing(&out).unwrap();
        assert_eq!(sink.texts(), ["Hello", "Hello, world"]);
        assert_eq!(chat.writing_due(), None, "nothing owed once sent");
        // Another item starts afresh.
        chat.wrote(&out, "m2", "Next").unwrap();
        assert_eq!(sink.texts().last().map(String::as_str), Some("Next"));
    }

    #[test]
    fn a_skill_named_goes_as_that_skill() {
        let listed = json!({ "data": [
            { "cwd": "/w", "skills": [
                { "name": "review", "description": "Review  code\nwell", "path": "/s/review/SKILL.md", "enabled": true, "scope": "user" },
                { "name": "off", "description": "", "path": "/s/off/SKILL.md", "enabled": false, "scope": "user" },
                { "name": "review-pr", "shortDescription": "PRs", "description": "long", "path": "/s/pr/SKILL.md", "enabled": true, "scope": "repo" }
            ], "errors": [] },
            { "cwd": "/w2", "skills": [
                { "name": "review", "description": "again", "path": "/s2/review/SKILL.md", "enabled": true, "scope": "user" }
            ], "errors": [] }
        ]});
        let skills = skills_of(&listed);
        assert_eq!(skills.iter().map(|s| (s.name.as_str(), s.description.as_str())).collect::<Vec<_>>(), [("review", "Review code well"), ("review-pr", "PRs")]);
        let input = user_input("please $review-pr now", &[], &skills);
        assert_eq!(input[1], json!({ "type": "skill", "name": "review-pr", "path": "/s/pr/SKILL.md" }));
        assert_eq!(input.as_array().unwrap().len(), 2, "$review-pr is not $review");
        assert!(names_skill("$review", "review") && names_skill("go ($review)", "review"));
        assert!(!names_skill("a$review", "review") && !names_skill("$reviewer", "review"));
    }

    #[test]
    fn what_a_page_leaves_out_comes_before_older_turns() {
        let mut chat = Chat {
            thread: "t".into(),
            joined: true,
            turn: None,
            state: AgentState::Idle,
            older: None,
            asks: HashMap::new(),
            edits: HashMap::new(),
            pending: HashMap::new(),
            model: None,
            effort: None,
            models: Vec::new(),
            next: None,
            held: Vec::new(),
            skills: Vec::new(),
            cwd: None,
            writing: None,
            rollout: Rollout::new(None),
        };
        let big = |n: usize| Item::Text {
            id: n.to_string(),
            text: "x".repeat(1 << 20),
        };
        let ids = |items: &[Item]| items.iter().map(|i| i.id().to_string()).collect::<Vec<_>>();
        let (page, more) = chat.page((0..6).map(big).collect());
        assert_eq!(ids(&page), ["3", "4", "5"]);
        assert!(more, "the rest is held");
        let held = std::mem::take(&mut chat.held);
        let (page, more) = chat.page(held);
        assert_eq!(ids(&page), ["0", "1", "2"]);
        assert!(!more, "nothing older");
    }

    #[test]
    fn models_and_settings_are_read_as_codex_gives_them() {
        let listed = json!({ "data": [
            { "id": "m1", "model": "gpt-5.5", "displayName": "GPT-5.5", "hidden": false,
              "supportedReasoningEfforts": [{ "reasoningEffort": "low", "description": "" }, { "reasoningEffort": "high", "description": "" }] },
            { "id": "m2", "model": "old", "displayName": "Old", "hidden": true, "supportedReasoningEfforts": [] }
        ]});
        let models = models_of(&listed);
        assert_eq!(models.len(), 1);
        assert_eq!((models[0].id.as_str(), models[0].name.as_str()), ("gpt-5.5", "GPT-5.5"));
        assert_eq!(models[0].efforts, ["low", "high"]);

        let mut chat = Chat {
            thread: "t".into(),
            joined: true,
            turn: None,
            state: AgentState::Idle,
            older: None,
            asks: HashMap::new(),
            edits: HashMap::new(),
            pending: HashMap::new(),
            model: None,
            effort: None,
            models,
            next: None,
            held: Vec::new(),
            skills: Vec::new(),
            cwd: None,
            writing: None,
            rollout: Rollout::new(None),
        };
        settings_of(&mut chat, &json!({ "model": "gpt-5.5", "reasoningEffort": "high" }));
        assert_eq!((chat.model.as_deref(), chat.effort.as_deref()), (Some("gpt-5.5"), Some("high")));
        settings_of(&mut chat, &json!({ "model": "gpt-5.5", "effort": "low" }));
        assert_eq!(chat.effort.as_deref(), Some("low"));
    }

    #[test]
    fn the_shell_codex_wraps_commands_in_is_taken_off() {
        assert_eq!(display_command("/bin/bash -lc 'touch a.txt'"), "touch a.txt");
        assert_eq!(display_command("bash -c \"echo \\\"hi\\\"\""), "echo \"hi\"");
        // Codex double-quotes a command with single quotes in it.
        assert_eq!(
            display_command(r#"/bin/bash -lc "printf 'one\\ntwo\\n'; echo \$HOME \`x\`""#),
            r#"printf 'one\ntwo\n'; echo $HOME `x`"#
        );
        assert_eq!(display_command("/bin/bash -lc 'a' && 'b'"), "'a' && 'b'");
        assert_eq!(display_command(r#"/bin/bash -lc "a" && "b""#), r#""a" && "b""#);
        assert_eq!(display_command(r#"/bin/bash -lc "a\""#), r#""a\""#);
        assert_eq!(display_command("cargo test"), "cargo test");
    }

    #[test]
    fn items_of_every_kind_become_chat_items() {
        let user = json!({"type":"userMessage","id":"u","content":[{"type":"text","text":"hi"}]});
        assert_eq!(
            to_item(&user),
            Some(Item::User {
                id: "u".into(),
                text: "hi".into(),
                attachments: vec![]
            })
        );
        let empty = json!({"type":"agentMessage","id":"a","text":""});
        assert_eq!(to_item(&empty), None);
        let reasoning = json!({"type":"reasoning","id":"r","summary":["**Looking**"],"content":[]});
        assert!(matches!(to_item(&reasoning), Some(Item::Thinking { text, .. }) if text == "**Looking**"));
        let failed = json!({"type":"commandExecution","id":"c","command":"/bin/bash -lc 'false'",
            "status":"completed","exitCode":1,"commandActions":[],"cwd":"/"});
        assert!(matches!(to_item(&failed), Some(Item::Command { status: Status::Failed, exit: Some(1), .. })));
        let read = json!({"type":"commandExecution","id":"c2","command":"sed -n 1,20p a.rs",
            "status":"completed","exitCode":0,"cwd":"/",
            "commandActions":[{"type":"read","command":"sed","name":"a.rs","path":"/x/a.rs"}]});
        assert!(matches!(to_item(&read), Some(Item::Command { looks, .. }) if looks.len() == 1 && looks[0].target == "a.rs"));
        let edit = json!({"type":"fileChange","id":"e","status":"completed","changes":[
            {"path":"a.rs","kind":{"type":"update"},"diff":"--- a\n+++ b\n@@ -1 +1,2 @@\n-x\n+y\n+z\n"}]});
        assert!(matches!(to_item(&edit), Some(Item::Edit { files, .. }) if files[0].added == 2 && files[0].removed == 1));
        let other = json!({"type":"somethingNew","id":"n"});
        assert_eq!(to_item(&other), None);
    }

    #[test]
    fn history_is_oldest_first_with_how_turns_ended() {
        let listed = json!({
            "data": [
                {"id":"t2","status":"interrupted","items":[{"type":"userMessage","id":"u2","content":[{"type":"text","text":"second"}]}]},
                {"id":"t1","status":"completed","items":[{"type":"userMessage","id":"u1","content":[{"type":"text","text":"first"}]}]}
            ],
            "nextCursor": "c"
        });
        let turns = turns_to_items(&listed);
        let ids: Vec<_> = turns.items.iter().map(Item::id).collect();
        assert_eq!(ids, ["u1", "u2", "t2-end"]);
        assert_eq!(turns.older, Some(json!("c")));
        assert_eq!(turns.running, None);
        let running = json!({"data": [{"id":"t3","status":"inProgress","items":[]}]});
        assert_eq!(turns_to_items(&running).running.as_deref(), Some("t3"));
    }

    #[test]
    fn a_thread_waiting_on_a_person_is_blocked() {
        assert_eq!(state_of(&json!({"type":"active","activeFlags":["waitingOnApproval"]})), AgentState::Blocked);
        assert_eq!(state_of(&json!({"type":"active","activeFlags":[]})), AgentState::Working);
        assert_eq!(state_of(&json!({"type":"idle"})), AgentState::Idle);
    }
}
