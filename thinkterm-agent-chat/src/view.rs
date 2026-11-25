//! The chat a client draws, made from what the bridge said: a row for each
//! thing said, what the agent did folded into one line each (looks at
//! files folded together), the question it waits on, and the state it is
//! in -- worded in the language the app shows.

use crate::markdown::{blocks, Block};
use crate::{
    diff, AgentState, Ask, AskKind, Attachment, Choice, CommandInfo, CommandKind, ContextInfo, Done, Ev, FileEdit, Item, Look, LookKind, ModelChoice, Need,
    NoteKind, PartKind, Status,
};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use thinkterm_i18n::{tr, tr_args, FluentArgs};

/// Items held at most; the oldest go first.
const ITEMS_KEPT: usize = 600;
/// Lines of a command's output, and of the diffs of an edit, in its fold.
const OUTPUT_LINES: usize = 80;
const DIFF_LINES: usize = 400;
/// Characters of a command in its fold's line.
const COMMAND_CHARS: usize = 60;

#[derive(Default)]
pub struct Chat {
    /// The pane it is with: in each view, so a client can tell a late
    /// view of the chat before from one of the chat it shows.
    pane: u32,
    agent: String,
    title: Option<String>,
    guessed: bool,
    items: Vec<Item>,
    more: bool,
    loading_older: bool,
    state: Option<AgentState>,
    since: Option<u64>,
    asks: Vec<Ask>,
    problem: Option<(Option<Need>, String)>,
    /// The notice on show, and the words it gives back to the field.
    notice: Option<(u64, String, Option<String>)>,
    /// Folds the person opened, by row id.
    open: HashSet<String>,
    /// Items held at most: [`ITEMS_KEPT`], and as many more as the person
    /// asked for from before them (`Op::Older`), which are not let go
    /// again to make room.
    limit: usize,
    /// The protocol the bridge speaks ([`crate::PROTOCOL`]).
    protocol: u32,
    /// The model the agent talks to, how hard it thinks, and the others.
    models: Option<(Option<String>, Option<String>, Vec<ModelChoice>)>,
    /// The small pictures the bridge sent, by key (`<item>#<n>`): the rows
    /// name them, and a client is given each once (`new_pictures`), not
    /// with every view.
    pictures: HashMap<String, String>,
    given: HashSet<String>,
    /// The commands the agent takes, as it said.
    commands: Vec<CommandInfo>,
    /// How full the context is, as the agent last said.
    context: Option<ContextInfo>,
    /// The bridge can have the agent take its extension in (`Op::Reload`).
    can_load: bool,
    /// The agent runs an older extension than the one in its folder.
    outdated: bool,
    /// Its reload command was sent: the greeting of the extension taken
    /// in anew is told as done.
    reloading: bool,
    /// When each item came about (unix seconds): as the bridge says for
    /// history, else when it first came here.
    times: HashMap<String, u64>,
    /// The items still being written (`Ev::Item::partial`).
    writing: HashSet<String>,
    /// What was last refused as a command only the terminal takes: sent
    /// again as it is, it goes, as words (`/tmp is full` is no command).
    refused: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChatView {
    /// The pane the chat is with.
    pub pane: u32,
    /// The agent's name as people know it: "Codex".
    pub agent: String,
    /// The conversation's own name, or "".
    pub title: String,
    /// `connecting`, `working`, `idle`, `blocked`, `problem`.
    pub state: &'static str,
    pub state_label: String,
    /// Unix seconds the state began, when known.
    pub since: Option<u64>,
    /// While it works: what it is doing ("Writing", "Running cargo test").
    pub busy: Option<String>,
    /// Said when the conversation was picked among several.
    pub guessed: Option<String>,
    pub rows: Vec<Row>,
    /// There is history before the first row.
    pub more: bool,
    pub more_label: String,
    pub loading_older: bool,
    pub ask: Option<AskView>,
    pub problem: Option<ProblemView>,
    /// Something to tell once: shown when `seq` is new.
    pub notice: Option<NoticeView>,
    pub placeholder: String,
    pub can_send: bool,
    pub can_stop: bool,
    /// Files and pictures can be sent along (a bridge that takes them).
    pub can_attach: bool,
    /// The model it talks to, and what it could be switched to; none when
    /// the agent does not say.
    pub model: Option<ModelView>,
    /// What a `/` (or `$`) in the field offers.
    pub commands: Vec<CommandView>,
    /// How full the model's context is: the ring in the bar, and the sheet
    /// it opens. None when the agent does not say.
    pub context: Option<ContextView>,
    /// The agent runs an older ThinkTerm extension than the one in its
    /// folder: said over the field, with the button that loads the new
    /// one (`load`).
    pub outdated: Option<OutdatedView>,
    /// When each row came about (unix seconds), by its id, where known.
    pub times: HashMap<String, u64>,
    /// The rows of what the agent is still writing, by id: shown as they
    /// grow, or held back until whole, as the app is set to.
    pub writing: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct OutdatedView {
    pub text: String,
    pub action: ActionView,
}

/// How full the context is, and what fills it.
#[derive(Debug, Serialize)]
pub struct ContextView {
    /// How much of the window is used, 0 to 1; none while not known.
    pub fill: Option<f32>,
    /// "ok", "high" (from 80%) or "full" (from 95%): the ring's colour.
    pub level: &'static str,
    /// "37%"; "–" while not known.
    pub percent: String,
    pub heading: String,
    /// "74.1k / 200k tokens".
    pub amount: String,
    /// What fills it, largest first, then what is free: shown on a tap of
    /// the row that says so (`breakdown`).
    pub parts: Vec<PartView>,
    pub breakdown: String,
    /// "Compacts by itself at 92%".
    pub compacts_at: Option<String>,
    /// How the parts were reckoned, or what the amount waits for.
    pub note: Option<String>,
    pub totals_heading: String,
    pub totals: Vec<StatView>,
    /// The button that compacts now, when the agent takes `/compact`.
    pub compact: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PartView {
    /// What it is ("system", "user", ... or "free"): its colour.
    pub kind: &'static str,
    pub label: String,
    /// "12.3k".
    pub amount: String,
    /// Its share of the window (of what is used, when the window is not
    /// known), 0 to 1.
    pub share: f32,
    /// That share, as said: "6%".
    pub percent: String,
}

#[derive(Debug, Serialize)]
pub struct StatView {
    pub label: String,
    pub value: String,
}

/// A command the field offers as it is typed.
#[derive(Debug, Serialize)]
pub struct CommandView {
    /// As typed: `/compact`.
    pub name: String,
    pub description: String,
    /// What follows it (`<name>`, or nothing said), when something may:
    /// a pick puts the command in the field for it rather than run it.
    pub args: Option<String>,
    /// Done by the client rather than sent: `model` opens the model menu.
    pub here: Option<&'static str>,
    /// `skill`, `command` or `prompt`: its icon, and the group it is in.
    pub kind: &'static str,
    /// Over the group it begins: "Skills". The commands come by group.
    pub group: String,
}

/// The model button and its menu: the models, each opening on how hard it
/// can think (a menu of its own), as iOS's menus nest.
#[derive(Debug, Serialize)]
pub struct ModelView {
    /// What the button says: "GPT-5.5 · High".
    pub label: String,
    /// "Model", over the models.
    pub models_heading: String,
    pub models: Vec<ModelOptionView>,
    /// "Thinking", over a model's efforts.
    pub efforts_heading: String,
}

#[derive(Debug, Serialize)]
pub struct ModelOptionView {
    /// What `set_model` takes.
    pub id: String,
    pub label: String,
    /// The one in use.
    pub current: bool,
    /// How hard it can think; the one in use marked, for the model in
    /// use. None: picked as it is.
    pub efforts: Vec<OptionView>,
}

#[derive(Debug, Serialize)]
pub struct OptionView {
    /// What `set_model` takes: a model's id, or an effort's own word.
    pub id: String,
    pub label: String,
    pub current: bool,
}

/// Something sent with what was said, as a client shows it.
#[derive(Debug, Serialize)]
pub struct AttachmentView {
    pub name: String,
    pub image: bool,
    /// The key of its small picture, when the bridge made one: given once
    /// (`Chat::new_pictures`).
    pub picture: Option<String>,
    /// Where it is on the agent's machine, when known.
    pub path: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Row {
    User {
        id: String,
        text: String,
        /// Pictures and files sent with it.
        attachments: Vec<AttachmentView>,
    },
    Text {
        id: String,
        blocks: Vec<Block>,
        /// The words as written, for the last reply of a turn: what a
        /// copy of the answer takes.
        #[serde(skip_serializing_if = "Option::is_none")]
        copy: Option<String>,
    },
    Fold {
        id: String,
        /// `think`, `look`, `run`, `edit`, `tool`.
        icon: &'static str,
        label: String,
        /// `running`, `done`, `failed`, `declined`, `stopped`.
        state: &'static str,
        /// A word on how it went, when not simply well ("exit 1").
        badge: Option<String>,
        added: Option<u32>,
        removed: Option<u32>,
        open: bool,
        /// There is something to show when it opens.
        can_open: bool,
        /// What it shows, while open: what a closed one holds (outputs,
        /// diffs) is not sent again with each change of the chat.
        body: Option<FoldBody>,
    },
    Note { id: String, text: String },
    /// What the agent did between two things said, two things or more of
    /// it, as one line ("Ran 3 commands, edited a file") that opens on
    /// them, each a fold of its own.
    Group {
        id: String,
        /// `run`, `look`, `edit`, `tool` when all it did was of one kind;
        /// else `group`.
        icon: &'static str,
        label: String,
        /// `running`, `stopped`, `done`.
        state: &'static str,
        /// How many of them failed, when some did ("1 failed"); for one
        /// thing done, its own ("declined", "exit 1").
        badge: Option<String>,
        /// For one change done, the lines it added and took out.
        #[serde(skip_serializing_if = "Option::is_none")]
        added: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        removed: Option<u32>,
        open: bool,
        /// Its folds, while open.
        rows: Vec<Row>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FoldBody {
    Text { blocks: Vec<Block> },
    Output { text: String },
    List { lines: Vec<String> },
    Diff { files: Vec<DiffFile> },
}

#[derive(Debug, Serialize)]
pub struct DiffFile {
    pub path: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Serialize)]
pub struct DiffLine {
    /// `add`, `del`, `ctx`, `hunk`.
    pub t: &'static str,
    pub s: String,
}

#[derive(Debug, Serialize)]
pub struct AskView {
    pub id: String,
    /// "Permission".
    pub label: String,
    /// "Run this command?"
    pub question: String,
    pub body: String,
    pub reason: Option<String>,
    /// "In ~/project".
    pub place: Option<String>,
    pub choices: Vec<ChoiceView>,
    /// With no choices: the label of the button that shows the terminal,
    /// where the agent takes its answer.
    pub terminal: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChoiceView {
    /// What `answer` is given: `accept`, `accept_for_session`, `decline`.
    pub id: &'static str,
    pub label: String,
    /// The one drawn as the main button.
    pub lead: bool,
}

#[derive(Debug, Serialize)]
pub struct ProblemView {
    pub text: String,
    /// The detail, in English, for whoever wants it.
    pub detail: String,
    pub action: Option<ActionView>,
}

#[derive(Debug, Serialize)]
pub struct ActionView {
    /// `install`, `load`, `retry`.
    pub id: &'static str,
    pub label: String,
}

#[derive(Debug, Serialize)]
pub struct NoticeView {
    pub seq: u64,
    pub text: String,
    /// What was typed and not sent: back in the field, to be put right.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept: Option<String>,
}

impl Chat {
    pub fn new(pane: u32, agent: &str) -> Self {
        Self {
            pane,
            agent: agent.to_string(),
            limit: ITEMS_KEPT,
            ..Default::default()
        }
    }

    /// Takes in one of the bridge's lines. Anything else on the channel
    /// (a login shell's greeting) is left aside: false.
    pub fn line(&mut self, line: &str) -> bool {
        match serde_json::from_str::<Ev>(line.trim()) {
            Ok(ev) => {
                self.apply(ev);
                true
            }
            Err(_) => false,
        }
    }

    pub fn apply(&mut self, ev: Ev) {
        match ev {
            Ev::Hello {
                agent,
                title,
                guessed,
                protocol,
            } => {
                self.agent = agent;
                self.protocol = protocol;
                // An extension's greeting is taken as current; the bridge
                // says after it when it is not (`Ev::Outdated`).
                self.outdated = false;
                // None is a conversation with no name (a new one, or one
                // whose name was taken off).
                self.title = title;
                // Said again on a rename, as not guessed: what was picked
                // among several stays so until a new channel.
                self.guessed |= guessed;
                self.problem = None;
                self.state.get_or_insert(AgentState::Idle);
                if std::mem::take(&mut self.reloading) {
                    let mut args = FluentArgs::new();
                    args.set("agent", display_name(&self.agent));
                    self.notify(tr_args("chat-reloaded", &args));
                }
            }
            Ev::History { mut items, more, reset } => {
                for item in &mut items {
                    self.keep_pictures(item);
                }
                if reset {
                    self.items = items;
                    self.limit = ITEMS_KEPT;
                    self.writing.clear();
                    // The conversation from the start: what it still asks is
                    // said again after it (an agent restarted asks nothing
                    // of before).
                    self.asks.clear();
                } else {
                    // Asked for: kept, whatever the room.
                    self.limit = self.limit.max(self.items.len()) + items.len();
                    let mut all = items;
                    all.append(&mut self.items);
                    self.items = all;
                }
                self.more = more;
                self.loading_older = false;
                self.trim();
            }
            Ev::Item { mut item, partial } => {
                self.keep_pictures(&mut item);
                let id = item.id().to_string();
                // Only a turn under way writes: one sent after it ended (a
                // last flush after a stop) is all there is of it.
                if partial && self.state == Some(AgentState::Working) {
                    self.writing.insert(id.clone());
                } else {
                    self.writing.remove(&id);
                }
                // As it comes: the bridge says when only for history.
                self.times.entry(id).or_insert_with(now);
                match self.items.iter_mut().rev().find(|held| held.id() == item.id()) {
                    Some(held) => *held = item,
                    None => self.items.push(item),
                }
                self.trim();
            }
            Ev::State { state, since } => {
                if self.state != Some(state) {
                    self.since = since;
                }
                self.state = Some(state);
                // At rest, nothing is still being written: what was is all
                // there will be of it (a turn stopped mid-sentence).
                if state != AgentState::Working {
                    self.writing.clear();
                }
            }
            Ev::Models { current, effort, models } => self.models = Some((current, effort, models)),
            Ev::Commands { commands } => self.commands = commands,
            Ev::Context { context } => self.context = Some(context),
            Ev::Ask { ask } => {
                self.asks.retain(|held| held.id != ask.id);
                self.asks.push(ask);
            }
            Ev::Resolved { id } => self.asks.retain(|held| held.id != id),
            Ev::Done { what: Done::Installed } => {
                let mut args = FluentArgs::new();
                args.set("agent", display_name(&self.agent));
                args.set("command", crate::reload_command(&self.agent));
                self.notify(tr_args("chat-installed", &args));
                self.problem = Some((Some(Need::PiRestart), String::new()));
            }
            Ev::Refused { message } => {
                // What was refused may be the older history asked for, or
                // the reload sent.
                self.loading_older = false;
                self.reloading = false;
                let mut args = FluentArgs::new();
                args.set("why", message);
                self.notify(tr_args("chat-refused", &args));
            }
            Ev::Error { need, message, can_load } => {
                self.problem = Some((need, message));
                self.can_load = can_load;
                self.writing.clear();
            }
            Ev::Outdated => self.outdated = true,
            Ev::Times { times } => self.times.extend(times),
            // Put after what it follows; with that not held, let go.
            Ev::Worked { id, after, seconds } => {
                self.items.retain(|item| item.id() != id);
                if let Some(at) = self.items.iter().rposition(|item| item.id() == after) {
                    self.items.insert(
                        at + 1,
                        Item::Note {
                            id,
                            note: NoteKind::Worked,
                            text: seconds.to_string(),
                        },
                    );
                }
            }
        }
    }

    /// The channel to the bridge ended: `missing` when the host's ThinkTerm
    /// has no bridge (an older one).
    pub fn ended(&mut self, missing: bool, reason: &str) {
        if missing {
            self.problem = Some((Some(Need::Bridge), reason.to_string()));
        } else if self.problem.is_none() {
            self.problem = Some((None, reason.to_string()));
        }
        self.asks.clear();
        // What came of a reply is all there will be on this channel.
        self.writing.clear();
    }

    /// A new channel is on its way: what was held stays on show until
    /// the bridge sends it again.
    pub fn reconnecting(&mut self) {
        self.problem = None;
        self.state = None;
        self.asks.clear();
        self.loading_older = false;
        self.guessed = false;
        // The channel to come says them again, if its agent still does.
        self.commands.clear();
        self.context = None;
        self.can_load = false;
        self.outdated = false;
        self.reloading = false;
        self.writing.clear();
    }

    /// The small pictures in `item`, kept by key and taken out of it.
    fn keep_pictures(&mut self, item: &mut Item) {
        if let Item::User { id, attachments, .. } = item {
            for (n, sent) in attachments.iter_mut().enumerate() {
                if let Some(thumb) = sent.thumb.take() {
                    self.pictures.insert(format!("{id}#{n}"), thumb);
                }
            }
        }
    }

    /// The small pictures no view has been given yet, by key, as `rows`
    /// name them: each once.
    pub fn new_pictures(&mut self) -> Vec<(String, String)> {
        let fresh: Vec<(String, String)> = self
            .pictures
            .iter()
            .filter(|(key, _)| !self.given.contains(*key))
            .map(|(key, data)| (key.clone(), data.clone()))
            .collect();
        self.given.extend(fresh.iter().map(|(key, _)| key.clone()));
        fresh
    }

    pub fn toggle(&mut self, id: &str) {
        if let Some(first) = id.strip_prefix("group:") {
            // A group open (by itself, or for a fold open in it) closes
            // with its folds.
            let members = self.run_from(first);
            let open = self.open.contains(id) || members.iter().any(|member| self.open.contains(member));
            if open {
                self.open.remove(id);
                for member in &members {
                    self.open.remove(member);
                }
            } else {
                self.open.insert(id.to_string());
            }
            return;
        }
        if !self.open.remove(id) {
            self.open.insert(id.to_string());
        } else if let Some(group) = self.group_holding(id) {
            // A fold closed in a group open for its sake: the group stays
            // open under the finger.
            self.open.insert(group);
        }
    }

    /// The group a fold is shown in, when its run of work is one: two
    /// things done or more (looks in a row are one).
    fn group_holding(&self, id: &str) -> Option<String> {
        let at = self.items.iter().position(|item| item.id() == id)?;
        if !is_work(&self.items[at]) {
            return None;
        }
        let mut start = at;
        while start > 0 && is_work(&self.items[start - 1]) {
            start -= 1;
        }
        // As `work_rows` makes them: a fold each, a run of looks one; two
        // or more are a group.
        let mut folds = 0;
        let mut looking = false;
        for item in self.items[start..].iter().take_while(|item| is_work(item)) {
            match item {
                Item::Command { looks, .. } if !looks.is_empty() => {
                    folds += usize::from(!looking);
                    looking = true;
                }
                _ => {
                    folds += 1;
                    looking = false;
                }
            }
        }
        (folds >= 2).then(|| format!("group:{}", self.items[start].id()))
    }

    /// The ids of the work done in a row from item `first` on.
    fn run_from(&self, first: &str) -> Vec<String> {
        let Some(at) = self.items.iter().position(|item| item.id() == first) else {
            return Vec::new();
        };
        self.items[at..]
            .iter()
            .take_while(|item| is_work(item))
            .map(|item| item.id().to_string())
            .collect()
    }

    pub fn loading_older(&mut self) {
        self.loading_older = true;
    }

    /// The ask for older history did not go out.
    pub fn older_failed(&mut self) {
        self.loading_older = false;
    }

    pub fn has_more(&self) -> bool {
        self.more && !self.loading_older
    }

    fn notify(&mut self, text: String) {
        self.notify_keeping(text, None);
    }

    fn notify_keeping(&mut self, text: String, kept: Option<String>) {
        let seq = self.notice.as_ref().map_or(1, |(seq, _, _)| seq + 1);
        self.notice = Some((seq, text, kept));
    }

    /// What does not fit goes, oldest first. The bridge is not asked for it
    /// again: a chat this long is read at its end. Nor for what came before
    /// it, which the bridge would send from where its own pages left off,
    /// past what went here.
    fn trim(&mut self) {
        if self.items.len() > self.limit {
            let extra = self.items.len() - self.limit;
            for item in self.items.drain(..extra) {
                self.times.remove(item.id());
            }
            self.more = false;
        }
    }

    pub fn view(&self) -> ChatView {
        let agent = display_name(&self.agent);
        let (state, state_label) = match (&self.problem, self.state) {
            (Some(_), _) => ("problem", String::new()),
            (None, None) => ("connecting", tr("chat-connecting")),
            (None, Some(AgentState::Working)) => ("working", tr("right-agents-state-working")),
            (None, Some(AgentState::Blocked)) => ("blocked", tr("right-agents-state-blocked")),
            (None, Some(AgentState::Idle)) => ("idle", tr("right-agents-state-idle")),
        };
        let resting = !matches!(self.state, Some(AgentState::Working) | Some(AgentState::Blocked));
        let rows = self.rows(resting);
        // A group's time is its first item's.
        let times = rows
            .iter()
            .filter_map(|row| {
                let id = row_id(row);
                let first = id.strip_prefix("group:").unwrap_or(id);
                self.times.get(first).map(|at| (id.to_string(), *at))
            })
            .collect();
        let writing = rows
            .iter()
            .filter(|row| matches!(row, Row::Text { .. }))
            .map(row_id)
            .filter(|id| self.writing.contains(*id))
            .map(str::to_string)
            .collect();
        let busy = (state == "working").then(|| match self.items.last() {
            Some(Item::Command {
                command,
                status: Status::Running,
                looks,
                ..
            }) if looks.is_empty() => {
                let mut args = FluentArgs::new();
                args.set("command", short_command(command));
                tr_args("chat-running", &args)
            }
            _ => tr("chat-writing"),
        });
        let guessed = self.guessed.then(|| {
            let mut args = FluentArgs::new();
            args.set("agent", agent.clone());
            tr_args("chat-guessed", &args)
        });
        let mut args = FluentArgs::new();
        args.set("agent", agent.clone());
        ChatView {
            title: self.title.clone().unwrap_or_default(),
            state,
            state_label,
            since: self.since,
            busy,
            guessed,
            rows,
            more: self.more,
            more_label: tr("chat-more"),
            loading_older: self.loading_older,
            ask: self.problem.is_none().then(|| self.asks.first().map(ask_view)).flatten(),
            problem: self
                .problem
                .as_ref()
                .map(|(need, detail)| problem_view(*need, detail, &self.agent, self.can_load)),
            notice: self.notice.as_ref().map(|(seq, text, kept)| NoticeView {
                seq: *seq,
                text: text.clone(),
                kept: kept.clone(),
            }),
            placeholder: tr_args("chat-placeholder", &args),
            pane: self.pane,
            can_send: self.problem.is_none() && self.state.is_some(),
            can_stop: self.problem.is_none() && self.state == Some(AgentState::Working),
            can_attach: self.problem.is_none() && self.protocol >= 2,
            commands: self.command_views(),
            context: self.context_view(),
            outdated: (self.outdated && self.problem.is_none()).then(|| {
                let mut args = FluentArgs::new();
                args.set("agent", display_name(&self.agent));
                OutdatedView {
                    text: tr_args("chat-outdated", &args),
                    action: ActionView {
                        id: "load",
                        label: tr_args("chat-load", &args),
                    },
                }
            }),
            model: self.model_view(),
            agent,
            times,
            writing,
        }
    }

    fn rows(&self, resting: bool) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut i = 0;
        while i < self.items.len() {
            if is_work(&self.items[i]) {
                let start = i;
                while i < self.items.len() && is_work(&self.items[i]) {
                    i += 1;
                }
                self.work_rows(&self.items[start..i], resting, &mut rows);
                continue;
            }
            if let Some(row) = self.row(&self.items[i], resting) {
                rows.push(row);
            }
            i += 1;
        }
        self.mark_answers(&mut rows, resting);
        rows
    }

    /// The last reply of each turn finished -- one the person spoke after,
    /// or the last of all once the agent rests -- carries its words to be
    /// copied whole.
    fn mark_answers(&self, rows: &mut [Row], resting: bool) {
        let mut ends = Vec::new();
        for (at, row) in rows.iter().enumerate() {
            if !matches!(row, Row::Text { .. }) {
                continue;
            }
            let next = rows[at + 1..].iter().find(|row| !matches!(row, Row::Note { .. }));
            if matches!(next, Some(Row::User { .. })) || (next.is_none() && resting) {
                ends.push(at);
            }
        }
        for at in ends {
            if let Row::Text { id, copy, .. } = &mut rows[at] {
                *copy = self.items.iter().rev().find_map(|item| match item {
                    Item::Text { id: held, text } if held == id => Some(text.clone()),
                    _ => None,
                });
            }
        }
    }

    /// The ring and its sheet, once the agent has said how full it is.
    fn context_view(&self) -> Option<ContextView> {
        let context = self.context.as_ref()?;
        let window = context.window.filter(|&w| w > 0);
        let fill = context.used.zip(window).map(|(used, window)| (used as f32 / window as f32).clamp(0.0, 1.0));
        let level = match fill {
            Some(f) if f >= 0.95 => "full",
            Some(f) if f >= 0.8 => "high",
            _ => "ok",
        };
        let mut parts: Vec<_> = context.parts.iter().filter(|part| part.tokens > 0).copied().collect();
        parts.sort_by_key(|part| std::cmp::Reverse(part.tokens));
        let whole = window.or(context.used).unwrap_or_else(|| parts.iter().map(|p| p.tokens).sum()).max(1);
        let part = |kind: &'static str, label: String, tokens: u64| {
            let share = (tokens as f32 / whole as f32).clamp(0.0, 1.0);
            PartView {
                kind,
                label,
                amount: token_count(tokens),
                share,
                percent: percent_of(share),
            }
        };
        let mut views: Vec<_> = parts.iter().map(|p| part(part_kind(p.kind), tr(part_label(p.kind)), p.tokens)).collect();
        if let (Some(used), Some(window)) = (context.used, window) {
            if window > used {
                views.push(part("free", tr("chat-context-free"), window - used));
            }
        }
        let amount = match (context.used, window) {
            (Some(used), Some(window)) => {
                let mut args = FluentArgs::new();
                args.set("used", token_count(used));
                args.set("window", token_count(window));
                tr_args("chat-context-amount", &args)
            }
            (Some(used), None) => {
                let mut args = FluentArgs::new();
                args.set("used", token_count(used));
                tr_args("chat-context-amount-alone", &args)
            }
            _ => tr("chat-context-unknown"),
        };
        let compacts_at = context.compacts_at.zip(window).map(|(at, window)| {
            let mut args = FluentArgs::new();
            args.set("percent", percent_of(at as f32 / window as f32));
            tr_args("chat-context-compacts-at", &args)
        });
        let note = (context.used.is_some() && !parts.is_empty()).then(|| tr("chat-context-estimated"));
        let totals = &context.totals;
        let mut stats = Vec::new();
        let mut stat = |key: &str, value: Option<String>| {
            if let Some(value) = value {
                stats.push(StatView { label: tr(key), value });
            }
        };
        // What an agent leaves at nothing (a model that does not count,
        // a cache never used) is not said.
        let some = |n: Option<u64>| n.filter(|&n| n > 0).map(token_count);
        stat("chat-context-input", some(totals.input));
        stat("chat-context-output", some(totals.output));
        stat("chat-context-cache-read", some(totals.cache_read));
        stat("chat-context-cache-write", some(totals.cache_write));
        stat("chat-context-reasoning", some(totals.reasoning));
        stat("chat-context-cost", totals.cost.filter(|c| *c > 0.0).map(dollars));
        let compact = (self.problem.is_none() && self.commands.iter().any(|c| c.name == "/compact")).then(|| tr("chat-context-compact"));
        Some(ContextView {
            fill,
            level,
            percent: fill.map_or_else(|| "–".to_string(), percent_of),
            heading: tr("chat-context-heading"),
            amount,
            parts: views,
            breakdown: tr("chat-context-breakdown"),
            compacts_at,
            note,
            totals_heading: tr("chat-context-totals"),
            totals: stats,
            compact,
        })
    }

    /// What the field offers on `/`, by group: the skills, the commands
    /// (the model menu first, when there is one), then the prompts --
    /// each group in the agent's own order.
    fn command_views(&self) -> Vec<CommandView> {
        let view = |command: &CommandInfo, kind: &'static str| CommandView {
            name: command.name.clone(),
            description: command.description.clone(),
            args: command.args.clone(),
            here: None,
            kind,
            group: String::new(),
        };
        let mut skills = Vec::new();
        let mut commands = Vec::new();
        let mut prompts = Vec::new();
        if self.models.as_ref().is_some_and(|(current, _, _)| current.is_some()) {
            commands.push(CommandView {
                name: "/model".into(),
                description: tr("chat-command-model"),
                args: None,
                here: Some("model"),
                kind: "command",
                group: String::new(),
            });
        }
        // Each name once: the lists key their rows by it.
        let mut seen = HashSet::new();
        for command in self.commands.iter().filter(|command| command.name != "/model" && seen.insert(command.name.as_str())) {
            match command_kind(command) {
                CommandKind::Skill => skills.push(view(command, "skill")),
                CommandKind::Prompt => prompts.push(view(command, "prompt")),
                CommandKind::Command | CommandKind::Other => commands.push(view(command, "command")),
            }
        }
        let mut views = Vec::new();
        for (mut group, key) in [(skills, "chat-commands-skills"), (commands, "chat-commands-commands"), (prompts, "chat-commands-prompts")] {
            let heading = tr(key);
            for view in &mut group {
                view.group = heading.clone();
            }
            views.append(&mut group);
        }
        views
    }

    /// Whether `text` may go to the agent. A command it does not take
    /// (`/goal`, which only its terminal knows) would reach the model as
    /// words: it is refused here, said why, given back to the field, and
    /// false. Sent again as it is, it goes, as words. Its reload command
    /// is kept in mind, to tell when the agent has taken it.
    pub fn may_send(&mut self, text: &str) -> bool {
        let again = self.refused.take().is_some_and(|refused| refused == text);
        let Some(name) = command_typed(text) else {
            return true;
        };
        // The agent has not said what it takes (an older bridge).
        if !again && !self.commands.is_empty() && !self.takes(name) {
            let mut args = FluentArgs::new();
            args.set("command", name.to_string());
            args.set("agent", display_name(&self.agent));
            self.notify_keeping(tr_args("chat-command-terminal", &args), Some(text.to_string()));
            self.refused = Some(text.to_string());
            return false;
        }
        if name == crate::reload_command(&self.agent) {
            self.reload_sent();
        }
        true
    }

    /// Whether the agent's bridge runs the command `name`: one it listed,
    /// or one it takes as typed without listing it. As they match them:
    /// in the case they are listed in.
    fn takes(&self, name: &str) -> bool {
        let unlisted: &[&str] = match self.agent.as_str() {
            "pi" | "omp" => &["/model", "/thinking"],
            "opencode" => &["/model", "/summarize"],
            _ => &["/model"],
        };
        self.commands.iter().any(|command| command.name == name) || unlisted.contains(&name)
    }

    /// The agent was asked to take its extension in anew: its next greeting
    /// is told as that done.
    pub fn reload_sent(&mut self) {
        self.reloading = true;
    }

    /// The model button and what it opens on, once the agent has said.
    fn model_view(&self) -> Option<ModelView> {
        let (current, effort, models) = self.models.as_ref()?;
        let current = current.as_deref()?;
        let known = models.iter().find(|model| model.id == current);
        let name = |model: &ModelChoice| if model.name.is_empty() { model.id.clone() } else { model.name.clone() };
        let label = match (known, effort) {
            (Some(model), Some(effort)) if !model.efforts.is_empty() => format!("{} · {}", name(model), effort_word(effort)),
            (Some(model), _) => name(model),
            (None, _) => current.to_string(),
        };
        let mut options: Vec<ModelOptionView> = models
            .iter()
            .map(|model| {
                let using = model.id == current;
                ModelOptionView {
                    id: model.id.clone(),
                    label: name(model),
                    current: using,
                    efforts: model
                        .efforts
                        .iter()
                        .map(|word| OptionView {
                            id: word.clone(),
                            label: effort_word(word),
                            current: using && effort.as_deref() == Some(word.as_str()),
                        })
                        .collect(),
                }
            })
            .collect();
        if known.is_none() {
            options.insert(
                0,
                ModelOptionView {
                    id: current.to_string(),
                    label: current.to_string(),
                    current: true,
                    efforts: Vec::new(),
                },
            );
        }
        Some(ModelView {
            label,
            models_heading: tr("chat-model"),
            models: options,
            efforts_heading: tr("chat-effort"),
        })
    }

    /// What the agent did between two things said: a fold each (looks at
    /// files folded together), or, when it did two things or more, one
    /// line for them all that opens on them.
    fn work_rows(&self, items: &[Item], resting: bool, rows: &mut Vec<Row>) {
        let looks_only = |item: &Item| matches!(item, Item::Command { looks, .. } if !looks.is_empty());
        let mut folds = Vec::new();
        let mut done = 0;
        let mut i = 0;
        while i < items.len() {
            if looks_only(&items[i]) {
                let start = i;
                while i < items.len() && looks_only(&items[i]) {
                    i += 1;
                }
                folds.push(self.looks_row(&items[start..i], resting));
                done += 1;
                continue;
            }
            if !matches!(items[i], Item::Thinking { .. }) {
                done += 1;
            }
            if let Some(row) = self.row(&items[i], resting) {
                folds.push(row);
            }
            i += 1;
        }
        // A run of work is one row however it grows: a fold alone, or one
        // line for all it holds. (Folds that turned into a group as more
        // came made the chat shrink and grow while the agent worked.)
        if folds.len() < 2 {
            rows.extend(folds);
            return;
        }
        let id = format!("group:{}", items[0].id());
        // A fold the person opened before more came to make a group of it
        // stays in view: the group is open with it.
        let open = self.open.contains(&id) || folds.iter().any(|row| matches!(row, Row::Fold { open: true, .. }));
        let tally = Tally::of(items);
        let running = items.iter().any(|item| match item {
            Item::Command { status, .. } | Item::Edit { status, .. } | Item::Tool { status, .. } => *status == Status::Running,
            _ => false,
        });
        // One thing done, with its thinking: that thing's own line.
        let single = (done <= 1)
            .then(|| {
                folds
                    .iter()
                    .rev()
                    .find(|row| matches!(row, Row::Fold { icon, .. } if *icon != "think" || done == 0))
            })
            .flatten();
        let group_badge = || {
            (tally.failed > 0).then(|| {
                let mut args = FluentArgs::new();
                args.set("count", tally.failed as i64);
                tr_args("chat-group-failed", &args)
            })
        };
        let group_state = if running { state_word(Status::Running, resting) } else { "done" };
        // That thing's own: its look, how it ended, what it changed.
        let (icon, label, state, badge, added, removed) = match single {
            Some(Row::Fold {
                icon,
                label,
                state,
                badge,
                added,
                removed,
                ..
            }) => (*icon, label.clone(), *state, badge.clone(), *added, *removed),
            _ => (tally.icon(), tally.label(), group_state, group_badge(), None, None),
        };
        rows.push(Row::Group {
            icon,
            label,
            state,
            badge,
            added,
            removed,
            open,
            rows: if open { folds } else { Vec::new() },
            id,
        });
    }

    fn fold(&self, id: &str, icon: &'static str, label: String, status: Status, resting: bool, more: Shown) -> Row {
        let open = self.open.contains(id);
        Row::Fold {
            id: id.to_string(),
            icon,
            label,
            state: state_word(status, resting),
            badge: more.badge,
            added: None,
            removed: None,
            open,
            can_open: more.can_open,
            body: more.body,
        }
    }

    fn row(&self, item: &Item, resting: bool) -> Option<Row> {
        Some(match item {
            Item::User { id, text, attachments } => {
                let (text, files) = uploads_named(text);
                let mut shown: Vec<AttachmentView> = attachments
                    .iter()
                    .enumerate()
                    .map(|(n, sent): (usize, &Attachment)| {
                        let key = format!("{id}#{n}");
                        AttachmentView {
                            name: sent.name.clone(),
                            image: sent.image,
                            picture: self.pictures.contains_key(&key).then_some(key),
                            path: sent.path.clone(),
                        }
                    })
                    .collect();
                shown.extend(files);
                Row::User {
                    id: id.clone(),
                    text,
                    attachments: shown,
                }
            }
            Item::Text { id, text } => Row::Text {
                id: id.clone(),
                blocks: blocks(text),
                copy: None,
            },
            Item::Thinking { id, text } => self.fold(
                id,
                "think",
                tr("chat-thought"),
                Status::Done,
                resting,
                self.shown(id, || FoldBody::Text { blocks: blocks(text) }),
            ),
            Item::Command {
                id,
                command,
                status,
                output,
                exit,
                ..
            } => {
                let mut args = FluentArgs::new();
                args.set("command", short_command(command));
                let badge = match (status, exit) {
                    (Status::Failed, Some(code)) => {
                        let mut args = FluentArgs::new();
                        args.set("code", i64::from(*code));
                        Some(tr_args("chat-exit", &args))
                    }
                    (Status::Declined, _) => Some(tr("chat-declined")),
                    _ => None,
                };
                // Not yet "ran" while it runs, or waits to be let run; turned
                // down, it never did: the command alone, beside its badge.
                let label = match status {
                    Status::Declined => short_command(command),
                    Status::Running if !resting => tr_args("chat-running", &args),
                    _ => tr_args("chat-ran", &args),
                };
                let shown = Shown {
                    badge,
                    ..self.shown(id, || {
                        let mut text = format!("$ {command}");
                        let shown = last_lines(output, OUTPUT_LINES);
                        if !shown.is_empty() {
                            text.push('\n');
                            text.push_str(&shown);
                        }
                        FoldBody::Output { text }
                    })
                };
                self.fold(id, "run", label, *status, resting, shown)
            }
            Item::Edit { id, status, files } => {
                // Not yet "edited" while it edits, or waits to be let edit.
                let going = *status == Status::Running && !resting;
                let label = match files.as_slice() {
                    // Turned down: the file alone, beside its badge.
                    [one] if *status == Status::Declined => file_name(&one.path),
                    [one] => {
                        let mut args = FluentArgs::new();
                        args.set("file", file_name(&one.path));
                        let key = match status {
                            _ if going => "chat-editing",
                            // It did not come about: not "edited".
                            Status::Failed => "chat-edit-failed",
                            _ => "chat-edited",
                        };
                        tr_args(key, &args)
                    }
                    many => {
                        let mut args = FluentArgs::new();
                        args.set("count", many.len() as i64);
                        let key = match status {
                            _ if going => "chat-editing-files",
                            Status::Failed => "chat-edit-failed-files",
                            _ => "chat-edited-files",
                        };
                        tr_args(key, &args)
                    }
                };
                let badge = (*status == Status::Declined).then(|| tr("chat-declined"));
                let shown = Shown {
                    badge,
                    ..self.shown(id, || FoldBody::Diff {
                        files: diff_files(files),
                    })
                };
                let mut row = self.fold(id, "edit", label, *status, resting, shown);
                if let Row::Fold { added, removed, .. } = &mut row {
                    *added = Some(files.iter().map(|f| f.added).sum());
                    *removed = Some(files.iter().map(|f| f.removed).sum());
                }
                row
            }
            Item::Tool {
                id,
                name,
                detail,
                status,
            } => {
                let label = match name.as_str() {
                    "web_search" => {
                        let mut args = FluentArgs::new();
                        args.set("query", detail.clone());
                        tr_args("chat-searched-web", &args)
                    }
                    "view_image" => tr("chat-viewed-image"),
                    "agent" => tr("chat-sub-agent"),
                    _ => {
                        let mut args = FluentArgs::new();
                        args.set("tool", name.clone());
                        tr_args("chat-used", &args)
                    }
                };
                let shown = if !detail.is_empty() && name != "web_search" {
                    self.shown(id, || FoldBody::Output { text: detail.clone() })
                } else {
                    Shown {
                        badge: None,
                        can_open: false,
                        body: None,
                    }
                };
                self.fold(id, "tool", label, *status, resting, shown)
            }
            Item::Note { id, note, text } => Row::Note {
                id: id.clone(),
                text: match note {
                    NoteKind::Compacted => tr("chat-note-compacted"),
                    NoteKind::Interrupted => tr("chat-note-interrupted"),
                    NoteKind::Failed => {
                        let mut args = FluentArgs::new();
                        args.set("why", text.clone());
                        tr_args("chat-note-failed", &args)
                    }
                    NoteKind::Worked => {
                        // A turn quicker than this is not worth a line.
                        let seconds = text.trim().parse::<u64>().ok().filter(|&s| s >= WORKED_SAID)?;
                        let mut args = FluentArgs::new();
                        args.set("time", duration_words(seconds));
                        tr_args("chat-note-worked", &args)
                    }
                    NoteKind::Other => return None,
                },
            },
        })
    }

    /// Runs that only looked at things, as one line: "Looked at 3 files".
    fn looks_row(&self, items: &[Item], resting: bool) -> Row {
        let mut files: Vec<&str> = Vec::new();
        let mut searches = 0i64;
        let mut lines = Vec::new();
        let mut status = Status::Done;
        for item in items {
            let Item::Command {
                looks, status: s, ..
            } = item
            else {
                continue;
            };
            if *s == Status::Running {
                status = Status::Running;
            }
            for Look { kind, target } in looks {
                match kind {
                    LookKind::Read | LookKind::List => {
                        if !files.contains(&target.as_str()) {
                            files.push(target);
                        }
                        lines.push(if *kind == LookKind::List {
                            format!("{target}/")
                        } else {
                            target.clone()
                        });
                    }
                    LookKind::Search => {
                        searches += 1;
                        lines.push(format!("“{target}”"));
                    }
                }
            }
        }
        let mut args = FluentArgs::new();
        args.set("files", files.len() as i64);
        args.set("searches", searches);
        let label = match (files.len(), searches) {
            (_, 0) => tr_args("chat-looked", &args),
            (0, _) => tr_args("chat-searched", &args),
            _ => tr_args("chat-looked-searched", &args),
        };
        let id = items.first().map(Item::id).unwrap_or_default();
        self.fold(id, "look", label, status, resting, self.shown(id, || FoldBody::List { lines }))
    }

    /// What a fold shows, made only while it is open.
    fn shown(&self, id: &str, body: impl FnOnce() -> FoldBody) -> Shown {
        Shown {
            badge: None,
            can_open: true,
            body: self.open.contains(id).then(body),
        }
    }
}

/// How hard a model thinks, in the app's words: the agents' own (`low`,
/// `xhigh`), each worded; one not known as it is.
fn effort_word(word: &str) -> String {
    match word {
        "none" | "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra" => tr(&format!("chat-effort-{word}")),
        other => other.to_string(),
    }
}

/// The folder a phone's files go to on the agent's machine.
const UPLOADS: &str = "/ThinkTerm_Uploads/";

/// What was said, and the files it ends with: the paths of files sent
/// from a phone, a line each, as the agent was told them.
fn uploads_named(text: &str) -> (String, Vec<AttachmentView>) {
    let mut lines: Vec<&str> = text.trim_end().lines().collect();
    let mut files = Vec::new();
    while let Some(last) = lines.last() {
        let path = last.trim().trim_matches(|c| c == '\'' || c == '"');
        if !path.starts_with('/') || !path.contains(UPLOADS) {
            break;
        }
        let name = path.rsplit('/').next().unwrap_or(path).to_string();
        let lower = name.to_ascii_lowercase();
        let image = [".png", ".jpg", ".jpeg", ".gif", ".webp", ".heic"].iter().any(|ext| lower.ends_with(ext));
        files.push(AttachmentView {
            name,
            image,
            picture: None,
            path: Some(path.to_string()),
        });
        lines.pop();
    }
    files.reverse();
    if files.is_empty() {
        return (text.to_string(), files);
    }
    (lines.join("\n").trim_end().to_string(), files)
}

/// Whether an item is something the agent did (thought, looked, ran,
/// changed, used), not something said.
fn is_work(item: &Item) -> bool {
    matches!(
        item,
        Item::Thinking { .. } | Item::Command { .. } | Item::Edit { .. } | Item::Tool { .. }
    )
}

/// What a run of work came to, for its one line: each kind counted, in the
/// order the agent first did it.
#[derive(Default)]
struct Tally {
    /// Kinds as first done: `look`, `search`, `run`, `edit`, `tool`.
    order: Vec<&'static str>,
    files_read: Vec<String>,
    searches: usize,
    ran: usize,
    files_edited: Vec<String>,
    tools: usize,
    failed: usize,
}

impl Tally {
    fn of(items: &[Item]) -> Self {
        let mut tally = Self::default();
        for item in items {
            match item {
                Item::Command { looks, status, .. } if looks.is_empty() => {
                    tally.saw("run");
                    tally.ran += 1;
                    tally.failed += usize::from(*status == Status::Failed);
                }
                Item::Command { looks, .. } => {
                    for look in looks {
                        match look.kind {
                            LookKind::Search => {
                                tally.saw("search");
                                tally.searches += 1;
                            }
                            LookKind::Read | LookKind::List => {
                                tally.saw("look");
                                if !tally.files_read.contains(&look.target) {
                                    tally.files_read.push(look.target.clone());
                                }
                            }
                        }
                    }
                }
                Item::Edit { files, status, .. } => {
                    tally.saw("edit");
                    for file in files {
                        if !tally.files_edited.contains(&file.path) {
                            tally.files_edited.push(file.path.clone());
                        }
                    }
                    tally.failed += usize::from(*status == Status::Failed);
                }
                Item::Tool { status, .. } => {
                    tally.saw("tool");
                    tally.tools += 1;
                    tally.failed += usize::from(*status == Status::Failed);
                }
                _ => {}
            }
        }
        tally
    }

    fn saw(&mut self, kind: &'static str) {
        if !self.order.contains(&kind) {
            self.order.push(kind);
        }
    }

    /// The one kind it was all of, as a fold's icon; else a group's.
    fn icon(&self) -> &'static str {
        match self.order.as_slice() {
            ["search"] | ["look", "search"] | ["search", "look"] => "look",
            [one] => one,
            _ => "group",
        }
    }

    /// "Ran 3 commands, edited a file".
    fn label(&self) -> String {
        let parts: Vec<String> = self
            .order
            .iter()
            .map(|kind| {
                let (key, count) = match *kind {
                    "look" => ("chat-group-read", self.files_read.len()),
                    "search" => ("chat-group-searched", self.searches),
                    "run" => ("chat-group-ran", self.ran),
                    "edit" => ("chat-group-edited", self.files_edited.len()),
                    _ => ("chat-group-used", self.tools),
                };
                let mut args = FluentArgs::new();
                args.set("count", count as i64);
                tr_args(key, &args)
            })
            .collect();
        let label = parts.join(&tr("chat-list-sep"));
        let mut chars = label.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect(),
            None => label,
        }
    }
}

/// What a fold shows besides its line: a word on how it went, and what
/// opening it shows -- made while it is open.
struct Shown {
    badge: Option<String>,
    can_open: bool,
    body: Option<FoldBody>,
}

fn state_word(status: Status, resting: bool) -> &'static str {
    match status {
        // Nothing runs while the agent rests: what still says it does was
        // cut off (an interrupted turn).
        Status::Running if resting => "stopped",
        Status::Running => "running",
        Status::Done => "done",
        Status::Failed => "failed",
        Status::Declined => "declined",
    }
}

fn ask_view(ask: &Ask) -> AskView {
    let place = ask.cwd.as_ref().map(|dir| {
        let mut args = FluentArgs::new();
        args.set("dir", short_dir(dir));
        tr_args("chat-ask-in", &args)
    });
    AskView {
        id: ask.id.clone(),
        label: tr("agent-kind-permission"),
        question: match ask.kind {
            AskKind::Command => tr("chat-ask-command"),
            AskKind::Edit => tr("chat-ask-edit"),
            AskKind::Tool => tr("chat-ask-tool"),
        },
        body: ask.body.clone(),
        reason: ask.reason.clone().filter(|r| !r.trim().is_empty()),
        place,
        choices: ask
            .choices
            .iter()
            .map(|choice| match choice {
                Choice::Accept => ChoiceView {
                    id: "accept",
                    label: tr("chat-choice-accept"),
                    lead: true,
                },
                Choice::AcceptForSession => ChoiceView {
                    id: "accept_for_session",
                    label: tr("chat-choice-session"),
                    lead: false,
                },
                Choice::Decline => ChoiceView {
                    id: "decline",
                    label: tr("chat-choice-decline"),
                    lead: false,
                },
            })
            .collect(),
        terminal: ask.choices.is_empty().then(|| tr("chat-ask-terminal")),
    }
}

fn problem_view(need: Option<Need>, detail: &str, agent_id: &str, can_load: bool) -> ProblemView {
    let retry = || {
        Some(ActionView {
            id: "retry",
            label: tr("chat-retry"),
        })
    };
    let terminal = || {
        Some(ActionView {
            id: "terminal",
            label: tr("chat-open-terminal"),
        })
    };
    let (key, action) = match need {
        Some(Need::PiExtension) => (
            "chat-need-pi-extension",
            Some(ActionView {
                id: "install",
                label: tr("chat-install"),
            }),
        ),
        // The bridge can have the agent take the extension in: one tap.
        Some(Need::PiRestart) if can_load => (
            "chat-need-pi-load",
            Some(ActionView {
                id: "load",
                label: {
                    let mut args = FluentArgs::new();
                    args.set("agent", display_name(agent_id));
                    tr_args("chat-load", &args)
                },
            }),
        ),
        Some(Need::PiRestart) => ("chat-need-pi-restart", retry()),
        Some(Need::CodexDaemon) => ("chat-need-codex-daemon", retry()),
        Some(Need::CodexThread) => ("chat-need-codex-thread", retry()),
        Some(Need::OpenCodeService) => ("chat-need-opencode-service", retry()),
        Some(Need::OpenCodeSession) => ("chat-need-opencode-session", retry()),
        Some(Need::OpenCodeUpgrade) => ("chat-need-opencode-upgrade", retry()),
        // Nothing to do here: the terminal is where the agent is.
        Some(Need::NotHere) => ("chat-need-not-here", terminal()),
        Some(Need::Unsupported) => ("chat-need-unsupported", terminal()),
        Some(Need::Bridge) => ("chat-need-bridge", terminal()),
        None => ("chat-ended", retry()),
    };
    ProblemView {
        text: {
            let mut args = FluentArgs::new();
            args.set("agent", display_name(agent_id));
            args.set("command", crate::reload_command(agent_id));
            tr_args(key, &args)
        },
        detail: detail.to_string(),
        action,
    }
}

/// A row's id.
fn row_id(row: &Row) -> &str {
    match row {
        Row::User { id, .. } | Row::Text { id, .. } | Row::Fold { id, .. } | Row::Note { id, .. } | Row::Group { id, .. } => id,
    }
}

/// Now, in unix seconds.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// What a command is: as its bridge says, else as its name tells.
fn command_kind(command: &CommandInfo) -> CommandKind {
    match command.kind {
        Some(kind) => kind,
        None if command.name.starts_with('$') || command.name.starts_with("/skill:") => CommandKind::Skill,
        None => CommandKind::Command,
    }
}

/// The command `text` begins with, when it begins with one: `/name`, a
/// word of its own (not a path such as `/usr/bin`).
fn command_typed(text: &str) -> Option<&str> {
    let word = text.split_whitespace().next()?;
    let name = word.strip_prefix('/')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))).then_some(word)
}

pub fn display_name(agent: &str) -> String {
    match agent {
        "codex" => "Codex".into(),
        "pi" => "Pi".into(),
        "omp" => "OMP".into(),
        "claude" => "Claude".into(),
        "opencode" => "OpenCode".into(),
        other => {
            let mut chars = other.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        }
    }
}

/// The first line of a command, cut short.
fn short_command(command: &str) -> String {
    let line = command.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(COMMAND_CHARS).collect();
    if line.chars().count() > COMMAND_CHARS || command.lines().nth(1).is_some() {
        out.push('…');
    }
    out
}

fn file_name(path: &str) -> String {
    path.rsplit('/').find(|part| !part.is_empty()).unwrap_or(path).to_string()
}

/// A folder as a person reads it: the home folder as `~`.
fn short_dir(dir: &str) -> String {
    for prefix in ["/home/", "/Users/"] {
        if let Some(rest) = dir.strip_prefix(prefix) {
            return match rest.find('/') {
                Some(at) => format!("~{}", &rest[at..]),
                None => "~".into(),
            };
        }
    }
    dir.to_string()
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn diff_files(files: &[FileEdit]) -> Vec<DiffFile> {
    let mut left = DIFF_LINES;
    files
        .iter()
        .map(|file| {
            let mut lines = Vec::new();
            for (kind, line) in diff::lines(&file.diff) {
                if left == 0 {
                    break;
                }
                let (t, s) = match kind {
                    diff::Line::Header => continue,
                    // A bare `@@` (a diff made up from what was asked) says
                    // nothing of where.
                    diff::Line::Hunk if line.trim() == "@@" => continue,
                    diff::Line::Hunk => ("hunk", line),
                    diff::Line::Added => ("add", &line[1..]),
                    diff::Line::Removed => ("del", &line[1..]),
                    diff::Line::Context => ("ctx", line.strip_prefix(' ').unwrap_or(line)),
                };
                lines.push(DiffLine { t, s: s.to_string() });
                left -= 1;
            }
            DiffFile {
                path: file.path.clone(),
                lines,
            }
        })
        .collect()
}

/// The shortest turn whose time is said ("Worked for 2 s").
const WORKED_SAID: u64 = 2;

/// A time, as said: "12 s", "1 min 5 s", "2 h 3 min".
fn duration_words(seconds: u64) -> String {
    let mut args = FluentArgs::new();
    match seconds {
        0..=59 => {
            args.set("s", seconds as i64);
            tr_args("chat-time-s", &args)
        }
        60..=3599 => {
            args.set("m", (seconds / 60) as i64);
            args.set("s", (seconds % 60) as i64);
            tr_args("chat-time-ms", &args)
        }
        _ => {
            args.set("h", (seconds / 3600) as i64);
            args.set("m", (seconds % 3600 / 60) as i64);
            tr_args("chat-time-hm", &args)
        }
    }
}

/// Tokens, short: 950, 1.2k, 74k, 1.3M.
fn token_count(n: u64) -> String {
    let short = |value: f64, unit: &str| {
        let shown = if value < 10.0 { format!("{value:.1}") } else { format!("{value:.0}") };
        format!("{}{unit}", shown.trim_end_matches(".0"))
    };
    match n {
        0..=999 => n.to_string(),
        1_000..=999_499 => short(n as f64 / 1_000.0, "k"),
        _ => short(n as f64 / 1_000_000.0, "M"),
    }
}

/// A share as said: "37%"; "<1%" for a little.
fn percent_of(share: f32) -> String {
    let percent = (share * 100.0).round();
    if percent < 1.0 && share > 0.0 {
        "<1%".to_string()
    } else {
        format!("{percent:.0}%")
    }
}

fn dollars(cost: f64) -> String {
    if cost < 0.01 {
        format!("${cost:.3}")
    } else {
        format!("${cost:.2}")
    }
}

fn part_kind(kind: PartKind) -> &'static str {
    match kind {
        PartKind::System => "system",
        PartKind::Tools => "tools",
        PartKind::Instructions => "instructions",
        PartKind::User => "user",
        PartKind::Assistant => "assistant",
        PartKind::Thinking => "thinking",
        PartKind::ToolUse => "tool_use",
        PartKind::Summary => "summary",
        PartKind::Other => "other",
    }
}

fn part_label(kind: PartKind) -> &'static str {
    match kind {
        PartKind::System => "chat-context-system",
        PartKind::Tools => "chat-context-tools",
        PartKind::Instructions => "chat-context-instructions",
        PartKind::User => "chat-context-user",
        PartKind::Assistant => "chat-context-assistant",
        PartKind::Thinking => "chat-context-thinking",
        PartKind::ToolUse => "chat-context-tool-use",
        PartKind::Summary => "chat-context-summary",
        PartKind::Other => "chat-context-other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(id: &str, command: &str, status: Status, looks: Vec<Look>) -> Item {
        Item::Command {
            id: id.into(),
            command: command.into(),
            status,
            output: String::new(),
            exit: None,
            looks,
        }
    }

    fn read(target: &str) -> Look {
        Look {
            kind: LookKind::Read,
            target: target.into(),
        }
    }

    #[test]
    fn looks_fold_together_and_running_work_stops_with_the_agent() {
        let mut chat = Chat::new(1, "codex");
        chat.apply(Ev::Hello {
            protocol: 1,
            agent: "codex".into(),
            title: Some("Fix it".into()),
            guessed: false,
        });
        chat.apply(Ev::History {
            items: vec![
                Item::User {
                    id: "u".into(),
                    text: "fix".into(),
                    attachments: vec![],
                },
                command("c1", "sed -n 1,9p a.rs", Status::Done, vec![read("a.rs")]),
                command("c2", "cat b.rs", Status::Done, vec![read("b.rs"), read("a.rs")]),
                command("c3", "cargo test", Status::Running, vec![]),
            ],
            more: true,
            reset: true,
        });
        chat.apply(Ev::State {
            state: AgentState::Working,
            since: Some(5),
        });
        // Two things done: one line, which opens on them.
        chat.toggle("group:c1");
        let view = chat.view();
        assert_eq!(view.agent, "Codex");
        assert_eq!(view.title, "Fix it");
        assert!(view.more && view.can_stop && view.can_send);
        assert_eq!(view.rows.len(), 2, "{:?}", view.rows);
        let Row::Group { state: "running", rows, .. } = &view.rows[1] else {
            panic!("not a running group: {:?}", view.rows[1]);
        };
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(matches!(&rows[0], Row::Fold { icon: "look", id, .. } if id == "c1"));
        assert!(matches!(&rows[1], Row::Fold { icon: "run", state: "running", .. }));
        assert!(view.busy.is_some());

        chat.apply(Ev::State {
            state: AgentState::Idle,
            since: Some(9),
        });
        let view = chat.view();
        let Row::Group { state: "stopped", rows, .. } = &view.rows[1] else {
            panic!("not a stopped group: {:?}", view.rows[1]);
        };
        assert!(matches!(&rows[1], Row::Fold { state: "stopped", .. }));
        assert!(view.busy.is_none() && !view.can_stop);
    }

    #[test]
    fn items_are_replaced_by_id_and_older_history_goes_first() {
        let mut chat = Chat::new(1, "pi");
        chat.apply(Ev::History {
            items: vec![command("c", "make", Status::Running, vec![])],
            more: true,
            reset: true,
        });
        chat.apply(Ev::item(command("c", "make", Status::Done, vec![])));
        chat.apply(Ev::History {
            items: vec![Item::User {
                id: "old".into(),
                text: "earlier".into(),
                attachments: vec![],
            }],
            more: false,
            reset: false,
        });
        let view = chat.view();
        assert_eq!(view.rows.len(), 2);
        assert!(matches!(&view.rows[0], Row::User { id, .. } if id == "old"));
        assert!(matches!(&view.rows[1], Row::Fold { state: "done", .. }));
        assert!(!view.more);
    }

    #[test]
    fn a_command_row_says_what_has_happened_yet() {
        let label = |status: Status, state: AgentState| {
            let mut chat = Chat::new(1, "codex");
            chat.apply(Ev::State { state, since: None });
            chat.apply(Ev::Item {
                item: Item::Command {
                    id: "c".into(),
                    command: "make".into(),
                    status,
                    output: String::new(),
                    exit: None,
                    looks: vec![],
                },
                partial: false,
            });
            match &chat.view().rows[0] {
                Row::Fold { label, .. } => label.clone(),
                other => panic!("not a fold: {other:?}"),
            }
        };
        assert_eq!(label(Status::Running, AgentState::Blocked), tr_args("chat-running", &{
            let mut args = FluentArgs::new();
            args.set("command", "make");
            args
        }));
        assert_ne!(label(Status::Done, AgentState::Idle), label(Status::Running, AgentState::Working));
        // Turned down: never ran.
        assert_eq!(label(Status::Declined, AgentState::Idle), "make");
    }

    #[test]
    fn asks_show_until_resolved_and_problems_hide_them() {
        let mut chat = Chat::new(1, "codex");
        chat.apply(Ev::State {
            state: AgentState::Blocked,
            since: None,
        });
        chat.apply(Ev::Ask {
            ask: Ask {
                id: "7".into(),
                kind: AskKind::Command,
                body: "rm -rf build".into(),
                reason: None,
                cwd: Some("/home/me/project".into()),
                choices: vec![Choice::Accept, Choice::AcceptForSession, Choice::Decline],
            },
        });
        let ask = chat.view().ask.unwrap();
        assert_eq!(ask.id, "7");
        assert_eq!(ask.choices.iter().filter(|c| c.lead).count(), 1);
        assert!(ask.place.unwrap().contains("~/project"));
        assert!(ask.terminal.is_none(), "answered here");
        chat.apply(Ev::Resolved { id: "7".into() });
        assert!(chat.view().ask.is_none());

        // OMP asks in its terminal only: shown, with the way there.
        chat.apply(Ev::Ask {
            ask: Ask {
                id: "t1".into(),
                kind: AskKind::Tool,
                body: "web_search {\"query\":\"x\"}".into(),
                reason: None,
                cwd: None,
                choices: vec![],
            },
        });
        let ask = chat.view().ask.unwrap();
        assert!(ask.choices.is_empty() && ask.terminal.is_some());
        chat.apply(Ev::Resolved { id: "t1".into() });
        assert!(chat.view().ask.is_none());

        chat.ended(true, "error: unrecognized subcommand 'chat'");
        let view = chat.view();
        assert_eq!(view.state, "problem");
        assert!(!view.can_send);
        // Nothing to do here but go to the terminal.
        assert_eq!(view.problem.unwrap().action.map(|a| a.id), Some("terminal"));
    }

    #[test]
    fn what_a_restarted_agent_asked_goes_with_its_history() {
        let mut chat = Chat::new(1, "omp");
        chat.apply(Ev::Ask {
            ask: Ask {
                id: "t1".into(),
                kind: AskKind::Command,
                body: "make".into(),
                reason: None,
                cwd: None,
                choices: vec![],
            },
        });
        assert!(chat.view().ask.is_some());
        // The bridge found the agent again, started anew: all it says is
        // what it still asks.
        chat.apply(Ev::History {
            items: vec![],
            more: false,
            reset: true,
        });
        assert!(chat.view().ask.is_none());
    }

    #[test]
    fn the_title_is_as_last_said_and_a_guess_lasts_the_channel() {
        let hello = |title: Option<&str>, guessed: bool| Ev::Hello {
            protocol: crate::PROTOCOL,
            agent: "codex".into(),
            title: title.map(str::to_string),
            guessed,
        };
        let mut chat = Chat::new(1, "codex");
        chat.apply(hello(Some("Fix login"), true));
        assert_eq!(chat.view().title, "Fix login");
        assert!(chat.view().guessed.is_some());
        // Renamed: said again, not guessed; the guess stands.
        chat.apply(hello(Some("Login"), false));
        assert_eq!(chat.view().title, "Login");
        assert!(chat.view().guessed.is_some());
        // A new conversation has no name.
        chat.apply(hello(None, false));
        assert_eq!(chat.view().title, "");
        chat.reconnecting();
        chat.apply(hello(None, false));
        assert!(chat.view().guessed.is_none());
    }

    #[test]
    fn a_chat_that_let_go_of_its_start_asks_for_no_more_before_it() {
        let mut chat = Chat::new(1, "pi");
        let said = |n: usize| Item::Text {
            id: format!("a{n}"),
            text: "ok".into(),
        };
        chat.apply(Ev::History {
            items: vec![said(0)],
            more: true,
            reset: true,
        });
        assert!(chat.view().more);
        for n in 1..=ITEMS_KEPT {
            chat.apply(Ev::item(said(n)));
        }
        // What the bridge would send next is from before c0, which went.
        let view = chat.view();
        assert_eq!(view.rows.len(), ITEMS_KEPT);
        assert!(!view.more);
    }

    #[test]
    fn work_between_two_things_said_is_one_line_that_opens_on_it() {
        let mut chat = Chat::new(1, "codex");
        let thought = Item::Thinking {
            id: "t".into(),
            text: "look first".into(),
        };
        let failed = Item::Command {
            id: "c2".into(),
            command: "make".into(),
            status: Status::Failed,
            output: String::new(),
            exit: Some(2),
            looks: vec![],
        };
        let edit = Item::Edit {
            id: "e".into(),
            status: Status::Done,
            files: vec![FileEdit {
                path: "src/a.rs".into(),
                added: 1,
                removed: 0,
                diff: "@@\n+x".into(),
            }],
        };
        let said = Item::Text {
            id: "a".into(),
            text: "Done.".into(),
        };
        chat.apply(Ev::History {
            items: vec![thought, command("c1", "make", Status::Done, vec![]), failed, edit, said],
            more: false,
            reset: true,
        });
        let view = chat.view();
        assert_eq!(view.rows.len(), 2, "{:?}", view.rows);
        match &view.rows[0] {
            Row::Group {
                id,
                icon,
                label,
                badge,
                open,
                rows,
                ..
            } => {
                assert_eq!(id, "group:t");
                assert_eq!(*icon, "group");
                let mut ran = FluentArgs::new();
                ran.set("count", 2i64);
                let mut edited = FluentArgs::new();
                edited.set("count", 1i64);
                let lower = format!(
                    "{}{}{}",
                    tr_args("chat-group-ran", &ran),
                    tr("chat-list-sep"),
                    tr_args("chat-group-edited", &edited)
                );
                assert_eq!(label.to_lowercase(), lower.to_lowercase());
                assert!(badge.is_some(), "one failed");
                assert!(!open && rows.is_empty());
            }
            other => panic!("not a group: {other:?}"),
        }
        chat.toggle("group:t");
        let open_rows = |chat: &Chat| match &chat.view().rows[0] {
            Row::Group { open, rows, .. } => (*open, rows.len()),
            other => panic!("not a group: {other:?}"),
        };
        assert_eq!(open_rows(&chat), (true, 4));
        // A fold opened in it keeps it open; closing the group closes both.
        chat.toggle("group:t");
        chat.toggle("c1");
        assert_eq!(open_rows(&chat), (true, 4));
        chat.toggle("group:t");
        assert_eq!(open_rows(&chat), (false, 0));
        // Open for a fold's sake, it stays open as that fold closes.
        chat.toggle("c2");
        assert_eq!(open_rows(&chat), (true, 4));
        chat.toggle("c2");
        assert_eq!(open_rows(&chat), (true, 4));

        // One thing done, with its thought: one line still -- the thing's
        // own -- so the chat does not grow by a line and shrink by two as
        // the agent goes on.
        let mut chat = Chat::new(1, "codex");
        chat.apply(Ev::History {
            items: vec![
                Item::Thinking {
                    id: "t".into(),
                    text: "x".into(),
                },
                command("c1", "make", Status::Done, vec![]),
            ],
            more: false,
            reset: true,
        });
        let rows = chat.view().rows;
        assert_eq!(rows.len(), 1);
        let Row::Group { label, icon, .. } = &rows[0] else { panic!("{rows:?}") };
        assert_eq!(*icon, "run");
        assert!(label.contains("make"), "{label}");
        // That one thing declined is said to be, as its own fold says.
        let mut chat = Chat::new(1, "codex");
        chat.apply(Ev::History {
            items: vec![
                Item::Thinking {
                    id: "t".into(),
                    text: "x".into(),
                },
                command("c1", "rm -rf build", Status::Declined, vec![]),
            ],
            more: false,
            reset: true,
        });
        let rows = chat.view().rows;
        let Row::Group { state, badge, .. } = &rows[0] else { panic!("{rows:?}") };
        assert_eq!(*state, "declined");
        assert!(badge.is_some());
        // Its thought opened and closed again: the group stays as it was.
        chat.toggle("group:t");
        chat.toggle("t");
        chat.toggle("t");
        assert!(matches!(&chat.view().rows[0], Row::Group { open: true, .. }));

        // Thinking alone is its own fold.
        let mut chat = Chat::new(1, "codex");
        chat.apply(Ev::Item {
            item: Item::Thinking {
                id: "t".into(),
                text: "x".into(),
            },
            partial: false,
        });
        assert!(matches!(chat.view().rows[..], [Row::Fold { .. }]));
    }

    #[test]
    fn a_turn_says_how_long_it_worked_when_that_was_a_while() {
        let said = |id: &str| Item::Text {
            id: id.into(),
            text: "done".into(),
        };
        let mut chat = Chat::new(1, "pi");
        chat.apply(Ev::History {
            items: vec![said("a"), said("b"), said("c"), said("d")],
            more: false,
            reset: true,
        });
        for (after, seconds) in [("a", 1), ("b", 12), ("c", 65), ("d", 3725), ("gone", 30)] {
            chat.apply(Ev::Worked {
                id: format!("w-{after}"),
                after: after.into(),
                seconds,
            });
        }
        // Said again, it stays one.
        chat.apply(Ev::Worked {
            id: "w-b".into(),
            after: "b".into(),
            seconds: 12,
        });
        let rows = chat.view().rows;
        // Each note right after what it follows.
        assert!(matches!(&rows[2], Row::Note { .. }) && matches!(&rows[1], Row::Text { .. }), "{rows:?}");
        let said: Vec<String> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Note { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(said.len(), 3, "a turn of a second is not worth a line, nor one after what is not held: {said:?}");
        assert!(said[0].contains("12"), "{said:?}");
        assert!(said[1].contains('1') && said[1].contains('5'), "{said:?}");
        assert!(said[2].contains('1') && said[2].contains('2'), "{said:?}");
    }

    #[test]
    fn files_sent_are_shown_beside_what_was_said() {
        let mut chat = Chat::new(1, "codex");
        chat.apply(Ev::Hello {
            protocol: 1,
            agent: "codex".into(),
            title: None,
            guessed: false,
        });
        assert!(!chat.view().can_attach, "an older bridge drops them");
        chat.apply(Ev::Hello {
            protocol: crate::PROTOCOL,
            agent: "codex".into(),
            title: None,
            guessed: false,
        });
        assert!(chat.view().can_attach);
        chat.apply(Ev::Item {
            item: Item::User {
                id: "u".into(),
                text: "look at these\n/home/me/ThinkTerm_Uploads/report.pdf\n'/home/me/ThinkTerm_Uploads/Photo 1.jpg'".into(),
                attachments: vec![Attachment {
                    name: "shot.png".into(),
                    image: true,
                    path: Some("/home/me/ThinkTerm_Uploads/shot.png".into()),
                    thumb: Some("AAAA".into()),
                }],
            },
            partial: false,
        });
        let view = chat.view();
        let Row::User { text, attachments, .. } = &view.rows[0] else {
            panic!("not a user row: {:?}", view.rows[0]);
        };
        assert_eq!(text, "look at these");
        let shown: Vec<_> = attachments.iter().map(|a| (a.name.as_str(), a.image, a.picture.clone())).collect();
        assert_eq!(
            shown,
            [
                ("shot.png", true, Some("u#0".to_string())),
                ("report.pdf", false, None),
                ("Photo 1.jpg", true, None)
            ]
        );
        // Its small picture goes once, by that key, not in the view.
        assert_eq!(chat.new_pictures(), [("u#0".to_string(), "AAAA".to_string())]);
        assert!(chat.new_pictures().is_empty());
        // A path said in passing is not taken for a file sent.
        assert_eq!(uploads_named("see /etc/hosts").0, "see /etc/hosts");
    }

    #[test]
    fn the_model_button_says_what_is_in_use_and_offers_the_rest() {
        let mut chat = Chat::new(1, "codex");
        assert!(chat.view().model.is_none(), "until the agent says");
        chat.apply(Ev::Models {
            current: Some("gpt-5.5".into()),
            effort: Some("high".into()),
            models: vec![
                ModelChoice {
                    id: "gpt-5.5".into(),
                    name: "GPT-5.5".into(),
                    efforts: vec!["low".into(), "high".into()],
                },
                ModelChoice {
                    id: "mini".into(),
                    name: String::new(),
                    efforts: vec![],
                },
            ],
        });
        let model = chat.view().model.unwrap();
        assert_eq!(model.label, format!("GPT-5.5 · {}", tr("chat-effort-high")));
        let models: Vec<_> = model.models.iter().map(|m| (m.id.as_str(), m.label.as_str(), m.current)).collect();
        assert_eq!(models, [("gpt-5.5", "GPT-5.5", true), ("mini", "mini", false)]);
        // Each model opens on its own efforts; the one in use is marked.
        let efforts: Vec<_> = model.models[0].efforts.iter().map(|e| (e.id.as_str(), e.current)).collect();
        assert_eq!(efforts, [("low", false), ("high", true)]);
        assert!(model.models[1].efforts.is_empty(), "picked as it is");

        // One the list does not have is shown all the same.
        chat.apply(Ev::Models {
            current: Some("custom".into()),
            effort: None,
            models: vec![],
        });
        let model = chat.view().model.unwrap();
        assert_eq!(model.label, "custom");
        assert!(model.models[0].current && model.models[0].efforts.is_empty());
    }

    #[test]
    fn the_field_offers_the_agents_commands_and_the_model_menu() {
        let mut chat = Chat::new(1, "pi");
        assert!(chat.view().commands.is_empty());
        chat.apply(Ev::Commands {
            commands: vec![
                CommandInfo {
                    name: "/compact".into(),
                    description: "Compact the context".into(),
                    args: None,
                    kind: None,
                },
                CommandInfo {
                    name: "/name".into(),
                    description: "Name the session".into(),
                    args: Some("<name>".into()),
                    kind: Some(CommandKind::Command),
                },
                CommandInfo {
                    name: "/fix-tests".into(),
                    description: "Fix what fails".into(),
                    args: Some(String::new()),
                    kind: Some(CommandKind::Prompt),
                },
                // An older extension says no kind: its name tells.
                CommandInfo {
                    name: "/skill:review".into(),
                    description: "Review".into(),
                    args: Some(String::new()),
                    kind: None,
                },
            ],
        });
        chat.apply(Ev::Models {
            current: Some("m".into()),
            effort: None,
            models: vec![],
        });
        let names: Vec<_> = chat.view().commands.iter().map(|c| (c.name.clone(), c.here, c.args.is_some(), c.kind)).collect();
        assert_eq!(
            names,
            [
                ("/skill:review".to_string(), None, true, "skill"),
                ("/model".to_string(), Some("model"), false, "command"),
                ("/compact".to_string(), None, false, "command"),
                ("/name".to_string(), None, true, "command"),
                ("/fix-tests".to_string(), None, true, "prompt"),
            ]
        );
        let groups: Vec<_> = chat.view().commands.iter().map(|c| c.group.clone()).collect();
        assert!(groups[0] != groups[1] && groups[1] == groups[3] && groups[3] != groups[4], "{groups:?}");
    }

    #[test]
    fn a_command_only_the_terminal_takes_is_refused_here() {
        let mut chat = Chat::new(1, "codex");
        // Nothing said of what it takes: everything goes.
        assert!(chat.may_send("/goal ship it"));
        chat.apply(Ev::Commands {
            commands: vec![CommandInfo {
                name: "/compact".into(),
                description: String::new(),
                args: None,
                kind: Some(CommandKind::Command),
            }],
        });
        assert!(chat.may_send("/compact"));
        assert!(chat.may_send("/model"));
        assert!(chat.may_send("please look at /usr/bin/env"));
        assert!(chat.may_send("/usr/bin/env is missing"));
        assert!(chat.may_send("$review this"));
        assert!(chat.view().notice.is_none());
        // Codex's own take only `/thinking`'s Pi.
        assert!(!chat.may_send("/thinking high"));
        // Matched as the bridge matches: `/COMPACT` would reach the model.
        assert!(!chat.may_send("/COMPACT"));
        assert!(!chat.may_send("/goal ship it"));
        let notice = chat.view().notice.unwrap();
        assert!(notice.text.contains("/goal"));
        assert_eq!(notice.kept.as_deref(), Some("/goal ship it"), "given back to the field");
        // Sent again as it is: words (`/tmp is full` is no command).
        assert!(!chat.may_send("/tmp is full"));
        assert!(chat.may_send("/tmp is full"));
        // Only right after: refused once more.
        assert!(!chat.may_send("/tmp is full"));
    }

    #[test]
    fn opencode_takes_summarize_and_pi_thinking_unlisted() {
        let compact = CommandInfo {
            name: "/compact".into(),
            description: String::new(),
            args: None,
            kind: Some(CommandKind::Command),
        };
        let mut opencode = Chat::new(1, "opencode");
        opencode.apply(Ev::Commands {
            commands: vec![compact.clone()],
        });
        assert!(opencode.may_send("/summarize"));
        let mut pi = Chat::new(2, "pi");
        pi.apply(Ev::Commands { commands: vec![compact] });
        assert!(pi.may_send("/thinking high"));
    }

    #[test]
    fn a_reload_refused_is_not_told_as_done() {
        let mut chat = Chat::new(1, "pi");
        chat.line(r#"{"ev":"hello","protocol":3,"agent":"pi"}"#);
        chat.reload_sent();
        chat.line(r#"{"ev":"refused","message":"/reload waits until Pi is done (or stopped)"}"#);
        let refused = chat.view().notice.unwrap().seq;
        // A rename's greeting, later.
        chat.line(r#"{"ev":"hello","protocol":3,"agent":"pi","title":"x"}"#);
        assert_eq!(chat.view().notice.unwrap().seq, refused, "no reload told");
    }

    #[test]
    fn a_reply_still_written_shows_once_the_channel_ends() {
        let mut chat = Chat::new(1, "pi");
        chat.line(r#"{"ev":"hello","protocol":3,"agent":"pi"}"#);
        chat.line(r#"{"ev":"state","state":"working"}"#);
        chat.line(r#"{"ev":"item","item":{"kind":"text","id":"t1","text":"half"},"partial":true}"#);
        assert_eq!(chat.view().writing, vec!["t1".to_string()]);
        chat.ended(false, "gone");
        assert!(chat.view().writing.is_empty());
        // A last flush after the turn ended is the reply whole.
        let mut late = Chat::new(1, "codex");
        late.line(r#"{"ev":"hello","protocol":3,"agent":"codex"}"#);
        late.line(r#"{"ev":"state","state":"idle"}"#);
        late.line(r#"{"ev":"item","item":{"kind":"text","id":"t2","text":"cut"},"partial":true}"#);
        assert!(late.view().writing.is_empty());
    }

    #[test]
    fn a_reload_sent_is_told_once_the_agent_greets_again() {
        let mut chat = Chat::new(1, "pi");
        chat.line(r#"{"ev":"hello","protocol":3,"agent":"pi"}"#);
        chat.apply(Ev::Commands {
            commands: vec![CommandInfo {
                name: "/reload".into(),
                description: String::new(),
                args: None,
                kind: Some(CommandKind::Command),
            }],
        });
        assert!(chat.may_send("/reload"));
        assert!(chat.view().notice.is_none());
        chat.line(r#"{"ev":"hello","protocol":3,"agent":"pi"}"#);
        let told = chat.view().notice.unwrap();
        // Once.
        chat.line(r#"{"ev":"hello","protocol":3,"agent":"pi"}"#);
        assert_eq!(chat.view().notice.unwrap().seq, told.seq);
    }

    #[test]
    fn the_last_reply_of_a_turn_can_be_copied_whole() {
        let mut chat = Chat::new(1, "pi");
        chat.line(r#"{"ev":"hello","protocol":3,"agent":"pi"}"#);
        chat.apply(Ev::History {
            items: vec![
                Item::User { id: "u1".into(), text: "hi".into(), attachments: vec![] },
                Item::Text { id: "t1".into(), text: "first **part**".into() },
                Item::Text { id: "t2".into(), text: "the answer".into() },
                Item::User { id: "u2".into(), text: "more".into(), attachments: vec![] },
                Item::Text { id: "t3".into(), text: "still going".into() },
            ],
            more: false,
            reset: true,
        });
        chat.apply(Ev::State { state: AgentState::Working, since: None });
        let copies = |chat: &Chat| -> Vec<Option<String>> {
            chat.view()
                .rows
                .into_iter()
                .filter_map(|row| match row {
                    Row::Text { copy, .. } => Some(copy),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(copies(&chat), [None, Some("the answer".to_string()), None]);
        chat.apply(Ev::State { state: AgentState::Idle, since: None });
        assert_eq!(copies(&chat), [None, Some("the answer".to_string()), Some("still going".to_string())]);
    }

    #[test]
    fn an_agent_started_before_the_extension_is_offered_to_load_it() {
        let mut chat = Chat::new(1, "pi");
        chat.line(r#"{"ev":"error","need":"pi_restart","message":"not running","can_load":true}"#);
        let problem = chat.view().problem.unwrap();
        assert_eq!(problem.action.map(|a| a.id), Some("load"));
        // An older bridge cannot: the person is told what to type.
        chat.reconnecting();
        chat.line(r#"{"ev":"error","need":"pi_restart","message":"not running"}"#);
        assert_eq!(chat.view().problem.unwrap().action.map(|a| a.id), Some("retry"));

        // Running an older extension: said over the field until a greeting
        // of one that is current.
        chat.reconnecting();
        chat.line(r#"{"ev":"hello","protocol":2,"agent":"pi"}"#);
        chat.line(r#"{"ev":"outdated"}"#);
        assert_eq!(chat.view().outdated.map(|o| o.action.id), Some("load"));
        chat.line(r#"{"ev":"hello","protocol":3,"agent":"pi"}"#);
        assert!(chat.view().outdated.is_none());
    }

    #[test]
    fn the_context_says_how_full_and_of_what() {
        let mut chat = Chat::new(1, "pi");
        assert!(chat.view().context.is_none(), "no ring until the agent says");
        chat.apply(Ev::Commands {
            commands: vec![CommandInfo {
                name: "/compact".into(),
                description: String::new(),
                args: None,
                kind: None,
            }],
        });
        chat.apply(Ev::Context {
            context: ContextInfo {
                used: Some(50_000),
                window: Some(200_000),
                parts: vec![
                    crate::ContextPart { kind: PartKind::User, tokens: 10_000 },
                    crate::ContextPart { kind: PartKind::System, tokens: 40_000 },
                ],
                compacts_at: Some(183_616),
                totals: crate::Usage {
                    input: Some(1_234_567),
                    output: Some(950),
                    cost: Some(0.4251),
                    ..Default::default()
                },
            },
        });
        let view = chat.view().context.unwrap();
        assert_eq!(view.fill, Some(0.25));
        assert_eq!(view.percent, "25%");
        assert_eq!(view.level, "ok");
        let parts: Vec<_> = view.parts.iter().map(|p| (p.kind, p.amount.as_str(), p.percent.as_str())).collect();
        assert_eq!(parts, [("system", "40k", "20%"), ("user", "10k", "5%"), ("free", "150k", "75%")]);
        assert!(view.compacts_at.unwrap().contains("92%"));
        let totals: Vec<_> = view.totals.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(totals, ["1.2M", "950", "$0.43"]);
        assert!(view.compact.is_some());
        assert_eq!(token_count(1_200), "1.2k");
        assert_eq!(token_count(74_100), "74k");
        assert_eq!(token_count(999_999), "1M");
        assert_eq!(percent_of(0.001), "<1%");
    }

    #[test]
    fn a_fold_holds_what_it_shows_only_while_open() {
        let mut chat = Chat::new(1, "pi");
        chat.apply(Ev::Item {
            item: Item::Command {
                id: "c".into(),
                command: "make".into(),
                status: Status::Done,
                output: "built".into(),
                exit: Some(0),
                looks: vec![],
            },
            partial: false,
        });
        let closed = |chat: &Chat| match &chat.view().rows[0] {
            Row::Fold { can_open, body, open, .. } => (*can_open, body.is_some(), *open),
            other => panic!("not a fold: {other:?}"),
        };
        assert_eq!(closed(&chat), (true, false, false));
        chat.toggle("c");
        assert_eq!(closed(&chat), (true, true, true));
        match &chat.view().rows[0] {
            Row::Fold {
                body: Some(FoldBody::Output { text }),
                ..
            } => assert_eq!(text, "$ make\nbuilt"),
            other => panic!("no output: {other:?}"),
        }
    }

    #[test]
    fn a_greeting_on_the_channel_is_not_a_line() {
        let mut chat = Chat::new(1, "codex");
        assert!(!chat.line("Welcome to Ubuntu"));
        assert!(chat.line(r#"{"ev":"state","state":"idle"}"#));
    }

    #[test]
    fn diffs_keep_their_lines_without_headers() {
        let files = diff_files(&[FileEdit {
            path: "a.rs".into(),
            added: 1,
            removed: 1,
            diff: "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n same".into(),
        }]);
        let kinds: Vec<_> = files[0].lines.iter().map(|l| (l.t, l.s.as_str())).collect();
        assert_eq!(kinds, [("hunk", "@@ -1 +1 @@"), ("del", "old"), ("add", "new"), ("ctx", "same")]);
        // Made up from what was written: no hunk to show, and a written
        // `++i;` is a line of the file.
        let files = diff_files(&[FileEdit {
            path: "b.c".into(),
            added: 2,
            removed: 0,
            diff: "@@\n+++i;\n+--j;".into(),
        }]);
        let kinds: Vec<_> = files[0].lines.iter().map(|l| (l.t, l.s.as_str())).collect();
        assert_eq!(kinds, [("add", "++i;"), ("add", "--j;")]);
    }

    #[test]
    fn commands_and_folders_are_shortened() {
        assert_eq!(short_command("cargo test\n--nocapture"), "cargo test…");
        assert_eq!(short_dir("/home/me/github/x"), "~/github/x");
        assert_eq!(short_dir("/srv/app"), "/srv/app");
        assert_eq!(file_name("src/view.rs"), "view.rs");
    }
}
