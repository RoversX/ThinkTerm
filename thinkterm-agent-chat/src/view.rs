//! The chat a client draws, made from what the bridge said: a row for each
//! thing said, what the agent did folded into one line each (looks at
//! files folded together), the question it waits on, and the state it is
//! in -- worded in the language the app shows.

use crate::markdown::{blocks, Block};
use crate::{
    diff, AgentState, Ask, AskKind, Attachment, Choice, CommandInfo, CommandKind, ContextInfo, Done, Ev, FileEdit, FileShown, Item, LoadWait, Look,
    LookKind, ModelChoice, Need, NoteKind, PartKind, Status,
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
/// Pictures under one line of what the agent did.
const PICTURES_SHOWN: usize = 8;

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
    /// How the agent takes its permission prompts, as its bridge said.
    mode: Option<String>,
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
    /// The bridge is having the agent take its extension in by itself, and
    /// waits for this (`Ev::Loading`): said while the chat connects.
    loading: Option<LoadWait>,
    /// The agent runs an older extension than the one in its folder.
    outdated: bool,
    /// Its reload command was sent: the greeting of the extension taken
    /// in anew is told as done.
    reloading: bool,
    /// When each item came about (unix seconds): as the bridge says for
    /// history, else when it first came here.
    times: HashMap<String, u64>,
    /// The files the bridge showed ([`Ev::File`]) that no client has taken
    /// yet (`take_files`): each given once, not with every view.
    files: Vec<FileView>,
    /// The items still being written (`Ev::Item::partial`).
    writing: HashSet<String>,
    /// What was last refused as a command only the terminal takes: sent
    /// again as it is, it goes, as words (`/tmp is full` is no command).
    refused: Option<String>,
    /// Cut off, and being opened again by itself (`lost`): said as
    /// reconnecting, not as a chat that ended, until the bridge greets.
    again: bool,
    /// What the person said that the agent has not taken yet, oldest
    /// first: on show at once at the end, faded, until the item it becomes
    /// comes; marked when it did not go, to be sent again.
    outgoing: Vec<Outgoing>,
    sent_count: u64,
    /// The last op that went to the bridge, when it was something said
    /// (`sent_as`): a refusal that comes next is of it.
    last_sent: Option<String>,
    /// Items of the person's that stand for something said here, by their
    /// id: shown under the id its outgoing row had, so the row stays.
    stand_ins: HashMap<String, String>,
    /// The last thing said that an item took, and that item's id: a bridge
    /// that shows what was said at once (Claude's) may still be refused it
    /// just after, and then it is put back as not sent.
    last_taken: Option<(String, Outgoing)>,
    /// The bridge greeted on this channel and has sent its history since:
    /// what is held goes only then, so that what the history shows arrived
    /// already is not sent twice.
    history_seen: bool,
}

/// Something said, on its way to the agent.
#[derive(Debug, Clone)]
struct Outgoing {
    id: String,
    text: String,
    attachments: Vec<Attachment>,
    /// Why it did not go, when it did not.
    failed: Option<String>,
    /// Said while the chat was not connected: it goes once it is
    /// (`take_held`), as nothing of it left the phone.
    held: bool,
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
    /// Words can be sent: while it connects too, as they are held and go
    /// once it is (`Chat::take_held`); not while something is wrong.
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
    /// What it is for, in the agent's words, when it says.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
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
        /// Not taken by the agent yet: `sending`, shown faded, or
        /// `failed`, shown as not sent, to be tapped to send again.
        #[serde(skip_serializing_if = "Option::is_none")]
        sending: Option<&'static str>,
        /// Said under one that failed: what a tap does.
        #[serde(skip_serializing_if = "Option::is_none")]
        failed: Option<String>,
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
        /// The pictures it looked at, under its line.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        pictures: Vec<AttachmentView>,
    },
    Note {
        id: String,
        text: String,
        /// Said at the side, small, as the end of a turn is ("Worked for
        /// 2m 49s"), rather than across the chat.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        aside: bool,
        /// When the turn it ends was done (unix seconds), where known: the
        /// client says it in its own words for a time.
        #[serde(skip_serializing_if = "Option::is_none")]
        done: Option<u64>,
    },
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
        /// The pictures its folds looked at, under its line while it is
        /// closed (open, each fold shows its own).
        #[serde(skip_serializing_if = "Vec::is_empty")]
        pictures: Vec<AttachmentView>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FoldBody {
    Text { blocks: Vec<Block> },
    Output { text: String },
    /// `paths`, beside `lines`: the file each line opens, empty for one
    /// that opens none (a search, a folder).
    List {
        lines: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        paths: Vec<String>,
    },
    Diff { files: Vec<DiffFile> },
}

/// A file the bridge showed ([`Ev::File`]), as a client lays it out.
#[derive(Debug, Serialize)]
pub struct FileView {
    /// As it was asked for: what the client asked for, to match it.
    pub path: String,
    /// Its own name, over it.
    pub name: String,
    /// How large it is, in words ("12 KB").
    pub size: String,
    /// `code` (its lines, coloured), `markdown` (laid out), `picture`, or
    /// `none` (`note` says why).
    pub kind: &'static str,
    /// Code: its lines, each in runs of one colour.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<Vec<CodeRun>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<Block>,
    /// The picture (JPEG or PNG, base64).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    /// Why it is not shown, or that only its start is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A run of code in one colour: `k`, what it is (`keyword`, `string`,
/// `comment`, ...), none for plain.
#[derive(Debug, Serialize, PartialEq)]
pub struct CodeRun {
    pub t: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub k: Option<String>,
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
    /// What a question asks: picks are sent with the `accept` choice
    /// (`answer_questions`), a question's picks joined by ", ". One
    /// question that takes one pick is answered by tapping it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<QuestionView>,
}

#[derive(Debug, Serialize)]
pub struct QuestionView {
    pub question: String,
    /// "Library".
    pub header: String,
    /// Several may be picked.
    pub multi: bool,
    pub options: Vec<QuestionOptionView>,
}

#[derive(Debug, Serialize)]
pub struct QuestionOptionView {
    pub label: String,
    pub description: String,
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
                // What is held was said to this agent, in the conversation
                // on show: one picked among several, or another agent in the
                // pane, is not sure to be it -- left for the person to send.
                if guessed || (self.protocol != 0 && agent != self.agent) {
                    self.give_up("the conversation changed");
                }
                self.history_seen = false;
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
                self.loading = None;
                self.again = false;
                // Said again after the greeting when known: one from before
                // (the agent started again in the pane) is not taken as it.
                self.mode = None;
                self.state.get_or_insert(AgentState::Idle);
                if std::mem::take(&mut self.reloading) {
                    let mut args = FluentArgs::new();
                    args.set("agent", display_name(&self.agent));
                    self.notify(tr_args("chat-reloaded", &args));
                }
            }
            Ev::History { mut items, more, reset } => {
                // Only an item new to the chat can be what was said here
                // just now: one held already, or from older pages, came
                // before it, whatever its words.
                let known: HashSet<String> = self.items.iter().map(|item| item.id().to_string()).collect();
                for item in &mut items {
                    self.keep_pictures(item);
                    if reset && !known.contains(item.id()) {
                        self.taken(item, false);
                    }
                }
                if reset {
                    self.history_seen = true;
                    self.items = items;
                    let held: HashSet<&str> = self.items.iter().map(Item::id).collect();
                    self.stand_ins.retain(|id, _| held.contains(id.as_str()));
                    self.limit = ITEMS_KEPT;
                    self.let_go_of_pictures();
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
                let fresh = !self.items.iter().any(|held| held.id() == id);
                self.taken(&item, fresh);
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
                // What was refused may be the older history asked for, the
                // reload sent, or what was just said: then marked as not
                // sent.
                let theirs = self.loading_older || self.reloading;
                self.loading_older = false;
                self.reloading = false;
                if let Some(id) = self.last_sent.take().filter(|_| !theirs) {
                    self.refused_send(&id, &message);
                }
                let mut args = FluentArgs::new();
                args.set("why", message);
                self.notify(tr_args("chat-refused", &args));
            }
            Ev::Error { need, message, can_load } => {
                // A card the person can act on (Load, Install) may still
                // lead to the chat: what is held waits for it.
                if can_load || matches!(need, Some(Need::PiRestart | Need::PiExtension)) {
                    self.unsure("the chat stopped");
                } else {
                    self.give_up("the chat stopped");
                }
                self.problem = Some((need, message));
                self.can_load = can_load;
                self.loading = None;
                self.writing.clear();
            }
            Ev::Outdated => self.outdated = true,
            // Nothing to put right: it connects once the agent takes it.
            Ev::Loading { wait } => {
                self.loading = Some(wait);
                self.problem = None;
                self.can_load = false;
                // A bridge saying so is a channel that is back.
                self.again = false;
            }
            Ev::Times { times } => self.times.extend(times),
            Ev::Mode { mode } => self.mode = Some(mode),
            Ev::File { file } => self.files.push(file_view(file)),
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
        self.give_up(reason);
        if missing {
            self.problem = Some((Some(Need::Bridge), reason.to_string()));
        } else if self.problem.is_none() {
            self.problem = Some((None, reason.to_string()));
        }
        self.again = false;
        self.asks.clear();
        // What came of a reply is all there will be on this channel.
        self.writing.clear();
    }

    /// The channel to the bridge went, and is being opened again by itself
    /// (the connection redials, or the channel is opened anew after a
    /// wait): said as reconnecting, what was held still on show, nothing
    /// asked of the person.
    pub fn lost(&mut self) {
        self.unsure("the connection dropped");
        self.history_seen = false;
        self.problem = None;
        self.state = None;
        self.asks.clear();
        self.writing.clear();
        self.loading_older = false;
        self.again = true;
    }

    /// Cut off and said as reconnecting (`lost`), not yet greeted again.
    pub fn reconnects(&self) -> bool {
        self.again && self.problem.is_none()
    }

    /// The bridge (or the channel's end) said what is wrong: a channel
    /// opened again would only say it again.
    pub fn has_problem(&self) -> bool {
        self.problem.is_some()
    }

    /// Shown again after the chat was left a while (kept, not made anew):
    /// what it hands a client once (small pictures, files, a notice) is
    /// handed the new one again, or not at all.
    pub fn shown_again(&mut self) {
        self.given.clear();
        self.files.clear();
        self.notice = None;
        self.refused = None;
    }

    /// A new channel is on its way: what was held stays on show until
    /// the bridge sends it again.
    pub fn reconnecting(&mut self) {
        // What went on the channel before is not sure to have reached the
        // agent; what is held goes on the new one.
        self.unsure("the chat was opened again");
        self.last_sent = None;
        self.history_seen = false;
        self.problem = None;
        self.state = None;
        self.asks.clear();
        self.loading_older = false;
        self.guessed = false;
        // The channel to come says them again, if its agent still does.
        self.commands.clear();
        self.context = None;
        self.can_load = false;
        self.loading = None;
        self.outdated = false;
        self.reloading = false;
        self.writing.clear();
    }

    /// The small pictures in `item`, kept by key and taken out of it.
    fn keep_pictures(&mut self, item: &mut Item) {
        let (id, pictures) = match item {
            Item::User { id, attachments, .. } => (id, attachments),
            Item::Command { id, pictures, .. } | Item::Tool { id, pictures, .. } => (id, pictures),
            _ => return,
        };
        for (n, sent) in pictures.iter_mut().enumerate() {
            if let Some(thumb) = sent.thumb.take() {
                self.pictures.insert(format!("{id}#{n}"), thumb);
            }
        }
    }

    /// The files the bridge showed since they were last taken, each once: a
    /// client shows them, and the next view does not carry them again.
    pub fn take_files(&mut self) -> Vec<FileView> {
        std::mem::take(&mut self.files)
    }

    /// The small pictures of items no longer held let go: a long chat of
    /// screenshots does not keep them all.
    fn let_go_of_pictures(&mut self) {
        let held: HashSet<&str> = self.items.iter().map(Item::id).collect();
        let kept = |key: &String| key.rsplit_once('#').is_some_and(|(id, _)| held.contains(id));
        self.pictures.retain(|key, _| kept(key));
        self.given.retain(kept);
    }

    /// The pictures of item `id`, as a client shows them: each by the key
    /// of its small picture, when there is one.
    fn picture_views(&self, id: &str, pictures: &[Attachment]) -> Vec<AttachmentView> {
        pictures
            .iter()
            .enumerate()
            .map(|(n, picture)| {
                let key = format!("{id}#{n}");
                AttachmentView {
                    name: picture.name.clone(),
                    image: true,
                    picture: self.pictures.contains_key(&key).then_some(key),
                    path: picture.path.clone(),
                }
            })
            .collect()
    }

    /// The pictures `items` looked at, together: the newest
    /// [`PICTURES_SHOWN`], in order.
    fn pictures_of(&self, items: &[Item]) -> Vec<AttachmentView> {
        let mut all = Vec::new();
        for item in items {
            if let Item::Command { id, pictures, .. } | Item::Tool { id, pictures, .. } = item {
                all.extend(self.picture_views(id, pictures));
            }
        }
        if all.len() > PICTURES_SHOWN {
            all.drain(..all.len() - PICTURES_SHOWN);
        }
        all
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
                if let Some(row) = self.stand_ins.remove(item.id()) {
                    self.times.remove(&row);
                }
            }
            self.let_go_of_pictures();
            self.more = false;
        }
    }

    pub fn view(&self) -> ChatView {
        let agent = display_name(&self.agent);
        let (state, state_label) = match (&self.problem, self.state) {
            (Some(_), _) => ("problem", String::new()),
            (None, None) if self.again => ("connecting", tr("chat-reconnecting")),
            // Why it is not connected yet, when the bridge is having the
            // agent take its extension in.
            (None, None) if self.loading.is_some() => {
                let key = match self.loading {
                    Some(LoadWait::Turn) => "chat-loading-after-turn",
                    Some(LoadWait::Field) => "chat-loading-after-field",
                    Some(LoadWait::Screen) => "chat-loading-after-screen",
                    _ => "chat-loading",
                };
                let mut args = FluentArgs::new();
                args.set("agent", agent.clone());
                ("connecting", tr_args(key, &args))
            }
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
            can_send: self.problem.is_none(),
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
        // Before what is on its way is put after it: a reply still being
        // written is no finished answer for a row the agent has not taken.
        self.mark_answers(&mut rows, resting);
        // What was said here and not taken yet, after all of it.
        for out in &self.outgoing {
            rows.push(Row::User {
                id: out.id.clone(),
                text: out.text.clone(),
                attachments: out
                    .attachments
                    .iter()
                    .map(|sent| AttachmentView {
                        name: sent.name.clone(),
                        image: sent.image,
                        picture: None,
                        path: sent.path.clone(),
                    })
                    .collect(),
                sending: Some(if out.failed.is_some() { "failed" } else { "sending" }),
                failed: out.failed.as_ref().map(|_| tr("chat-not-sent")),
            });
        }
        rows
    }

    /// The agent can be spoken to now: greeted, and nothing wrong.
    pub fn connected(&self) -> bool {
        self.problem.is_none() && self.state.is_some()
    }

    /// The person said `text`, with `attachments`: on show at once as
    /// sending, until the agent takes it. Its id, for whether it went
    /// (`sent_as`, `not_sent`) -- none for a command (`/compact`), which no
    /// agent shows as said; words that only start with a slash (a path) or
    /// a dollar sign are words. Said while the chat is not connected, it is
    /// held, to go once it is (`take_held`).
    pub fn sending(&mut self, text: &str, attachments: &[Attachment]) -> Option<String> {
        let words = text.trim();
        if attachments.is_empty() && (words.is_empty() || command_typed(words).is_some()) {
            return None;
        }
        self.sent_count += 1;
        let id = format!("outgoing-{}", self.sent_count);
        self.times.insert(id.clone(), now());
        self.outgoing.push(Outgoing {
            id: id.clone(),
            text: text.to_string(),
            attachments: attachments.to_vec(),
            failed: None,
            held: !self.connected(),
        });
        Some(id)
    }

    /// The op that just went to the bridge: what was said as `id`, or
    /// something else (`None`). A refusal that comes next is of it.
    pub fn sent_as(&mut self, id: Option<String>) {
        self.last_sent = id;
    }

    /// What was said and held while the chat was not connected, to go now
    /// that it is, and the bridge has sent its history (which shows what
    /// arrived already): id, words and files. None until then.
    pub fn take_held(&mut self) -> Vec<(String, String, Vec<Attachment>)> {
        if !self.connected() || !self.history_seen {
            return Vec::new();
        }
        self.outgoing
            .iter_mut()
            .filter(|out| out.held && out.failed.is_none())
            .map(|out| {
                out.held = false;
                (out.id.clone(), out.text.clone(), out.attachments.clone())
            })
            .collect()
    }

    /// What was said as `id` could not go out now: held, to go once the
    /// chat is connected again.
    pub fn hold(&mut self, id: &str) {
        if let Some(out) = self.outgoing.iter_mut().find(|out| out.id == id) {
            out.held = true;
        }
    }

    /// What was said as `id` did not go (`why`, in English): marked, to be
    /// sent again.
    pub fn not_sent(&mut self, id: &str, why: &str) {
        if let Some(out) = self.outgoing.iter_mut().find(|out| out.id == id) {
            out.failed = Some(why.to_string());
            out.held = false;
        }
    }

    /// The bridge refused what was said as `id`, just after it went: marked
    /// as not sent -- put back first when a bridge that shows what was said
    /// at once (Claude's) had shown it already.
    fn refused_send(&mut self, id: &str, why: &str) {
        if !self.outgoing.iter().any(|out| out.id == id) {
            let Some((item, mut out)) = self.last_taken.take().filter(|(_, out)| out.id == id) else {
                return;
            };
            self.items.retain(|held| held.id() != item);
            self.stand_ins.remove(&item);
            out.held = false;
            self.outgoing.push(out);
        }
        self.not_sent(id, why);
    }

    /// A command typed while the chat is not connected: not sent, and back
    /// in the field to send once it is. A reload it was is not on its way.
    pub fn command_not_sent(&mut self, text: &str) {
        self.reloading = false;
        self.notify_keeping(tr("chat-not-connected"), Some(text.to_string()));
    }

    /// What was said as `id`, to be sent again: its words and files to send
    /// now, or none while the chat is not connected -- it is held then, to
    /// go once it is.
    pub fn resend(&mut self, id: &str) -> Option<(String, Vec<Attachment>)> {
        let ready = self.connected() && self.history_seen;
        let out = self.outgoing.iter_mut().find(|out| out.id == id && out.failed.is_some())?;
        out.failed = None;
        out.held = !ready;
        ready.then(|| (out.text.clone(), out.attachments.clone()))
    }

    /// Nothing said here that went out and is still on its way is sure to
    /// have reached the agent: marked, to be sent again -- what did reach
    /// it is taken from the history the channel sends anew. What is held
    /// never left, and still goes once the chat is back.
    fn unsure(&mut self, why: &str) {
        for out in self.outgoing.iter_mut().filter(|out| !out.held) {
            out.failed.get_or_insert_with(|| why.to_string());
        }
    }

    /// The chat stopped, and does not come back by itself: all that is on
    /// its way, held or not, is marked, to be sent again.
    fn give_up(&mut self, why: &str) {
        for out in &mut self.outgoing {
            out.held = false;
            out.failed.get_or_insert_with(|| why.to_string());
        }
        self.last_sent = None;
    }

    /// An item of the person's came (`fresh`: just now, not in history):
    /// what was said here and is it goes from the rows on their way -- the
    /// one with its words; or, for one just come, the oldest on its way with
    /// files, whose words a bridge puts its own way (their paths after them).
    /// What is held never left the phone, and is no item's.
    fn taken(&mut self, item: &Item, fresh: bool) {
        let Item::User { id, text, .. } = item else {
            return;
        };
        if self.outgoing.is_empty() || self.stand_ins.contains_key(id) {
            return;
        }
        let words = uploads_named(text).0;
        let at = self
            .outgoing
            .iter()
            .position(|out| !out.held && out.text.trim() == words.trim())
            .or_else(|| {
                fresh
                    .then(|| {
                        self.outgoing
                            .iter()
                            .position(|out| !out.held && out.failed.is_none() && !out.attachments.is_empty())
                    })
                    .flatten()
            });
        if let Some(at) = at {
            let out = self.outgoing.remove(at);
            self.stand_ins.insert(id.clone(), out.id.clone());
            self.last_taken = Some((id.clone(), out));
        }
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
        let mut label = match (known, effort) {
            (Some(model), Some(effort)) if !model.efforts.is_empty() => format!("{} · {}", name(model), effort_word(effort)),
            (Some(model), _) => name(model),
            (None, _) => current.to_string(),
        };
        if let Some(mode) = &self.mode {
            label = format!("{label} · {}", mode_word(mode));
        }
        let mut options: Vec<ModelOptionView> = models
            .iter()
            .map(|model| {
                let using = model.id == current;
                ModelOptionView {
                    id: model.id.clone(),
                    label: name(model),
                    description: model.description.clone(),
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
                    description: String::new(),
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
            pictures: if open { Vec::new() } else { self.pictures_of(items) },
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
            pictures: Vec::new(),
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
                    // Where it stands for something said here, the row that
                    // was shown for that.
                    id: self.stand_ins.get(id).cloned().unwrap_or_else(|| id.clone()),
                    text,
                    attachments: shown,
                    sending: None,
                    failed: None,
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
                pictures,
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
                let mut row = self.fold(id, "run", label, *status, resting, shown);
                if let Row::Fold { pictures: shown, .. } = &mut row {
                    *shown = self.picture_views(id, pictures);
                }
                row
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
                pictures,
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
                let mut row = self.fold(id, "tool", label, *status, resting, shown);
                if let Row::Fold { pictures: shown, .. } = &mut row {
                    *shown = self.picture_views(id, pictures);
                }
                row
            }
            Item::Note { id, note, text } => {
                let across = |text: String| Row::Note {
                    id: id.clone(),
                    text,
                    aside: false,
                    done: None,
                };
                match note {
                    NoteKind::Compacted => across(tr("chat-note-compacted")),
                    NoteKind::Interrupted => across(tr("chat-note-interrupted")),
                    NoteKind::Failed => {
                        let mut args = FluentArgs::new();
                        args.set("why", text.clone());
                        across(tr_args("chat-note-failed", &args))
                    }
                    NoteKind::Worked => {
                        // A turn quicker than this is not worth a line.
                        let seconds = text.trim().parse::<u64>().ok().filter(|&s| s >= WORKED_SAID)?;
                        let mut args = FluentArgs::new();
                        args.set("time", duration_words(seconds));
                        Row::Note {
                            id: id.clone(),
                            text: tr_args("chat-note-worked", &args),
                            aside: true,
                            done: self.done_at(id, seconds),
                        }
                    }
                    NoteKind::Other => return None,
                }
            }
        })
    }

    /// When the turn whose note is `id`, `seconds` long, was done: from
    /// when it was asked (the person's words before it), as long as that
    /// falls between its last reply and what came next; else when its
    /// last reply came (words sent while it worked would put it late).
    fn done_at(&self, id: &str, seconds: u64) -> Option<u64> {
        let at = self.items.iter().position(|item| item.id() == id)?;
        let last = at.checked_sub(1).and_then(|before| self.times.get(self.items[before].id())).copied();
        let next = self.items[at + 1..]
            .iter()
            .find_map(|item| self.times.get(item.id()))
            .copied()
            .unwrap_or_else(now);
        let asked = self.items[..at]
            .iter()
            .rev()
            .find(|item| matches!(item, Item::User { .. }))
            .and_then(|item| self.times.get(item.id()));
        let reckoned = asked.and_then(|asked| asked.checked_add(seconds));
        match (reckoned, last) {
            (Some(done), Some(last)) if done >= last && done <= next => Some(done),
            (Some(done), None) if done <= next => Some(done),
            (_, last) => last,
        }
    }

    /// Runs that only looked at things, as one line: "Looked at 3 files".
    fn looks_row(&self, items: &[Item], resting: bool) -> Row {
        let mut files: Vec<&str> = Vec::new();
        let mut searches = 0i64;
        let mut lines = Vec::new();
        // The file each line opens: one read, not a folder or a search.
        let mut paths = Vec::new();
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
            for Look { kind, target, path } in looks {
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
                        paths.push(if *kind == LookKind::Read { path.clone().unwrap_or_else(|| target.clone()) } else { String::new() });
                    }
                    LookKind::Search => {
                        searches += 1;
                        lines.push(format!("“{target}”"));
                        paths.push(String::new());
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
        if paths.iter().all(String::is_empty) {
            paths.clear();
        }
        let mut row = self.fold(id, "look", label, status, resting, self.shown(id, || FoldBody::List { lines, paths }));
        if let Row::Fold { pictures, .. } = &mut row {
            *pictures = self.pictures_of(items);
        }
        row
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

/// A permission mode as the client says it: Claude Code's words for them.
fn mode_word(mode: &str) -> String {
    match mode {
        "default" => tr("chat-mode-default"),
        "acceptEdits" => tr("chat-mode-accept-edits"),
        "plan" => tr("chat-mode-plan"),
        "auto" => tr("chat-mode-auto"),
        "bypassPermissions" => tr("chat-mode-bypass"),
        "dontAsk" => tr("chat-mode-dont-ask"),
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
    let asking = ask.kind == AskKind::Question;
    AskView {
        id: ask.id.clone(),
        label: tr(if asking { "agent-kind-question" } else { "agent-kind-permission" }),
        question: match ask.kind {
            AskKind::Command => tr("chat-ask-command"),
            AskKind::Edit => tr("chat-ask-edit"),
            AskKind::Tool => tr("chat-ask-tool"),
            // One question is the card's own; several are listed under it.
            AskKind::Question => match &ask.questions[..] {
                [only] => only.question.clone(),
                _ => tr("chat-ask-questions"),
            },
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
                    label: tr(if asking { "chat-choice-answer" } else { "chat-choice-accept" }),
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
        questions: ask
            .questions
            .iter()
            .map(|q| QuestionView {
                question: q.question.clone(),
                header: q.header.clone(),
                multi: q.multi,
                options: q
                    .options
                    .iter()
                    .map(|o| QuestionOptionView {
                        label: o.label.clone(),
                        description: o.description.clone(),
                    })
                    .collect(),
            })
            .collect(),
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

/// A file the bridge showed, laid out: code in lines of coloured runs,
/// Markdown in blocks, a picture as it is, or the words for why not.
fn file_view(file: FileShown) -> FileView {
    let mut view = FileView {
        name: file_name(&file.path),
        size: size_words(file.size),
        path: file.path,
        kind: "none",
        lines: Vec::new(),
        blocks: Vec::new(),
        picture: None,
        note: None,
    };
    if let Some(error) = file.error {
        view.note = Some(tr(match error.as_str() {
            "missing" => "chat-file-missing",
            "folder" => "chat-file-folder",
            "binary" => "chat-file-binary",
            "too_big" => "chat-file-too-big",
            _ => "chat-file-unreadable",
        }));
        return view;
    }
    if let Some(picture) = file.picture {
        view.kind = "picture";
        view.picture = Some(picture);
        return view;
    }
    let text = file.text.unwrap_or_default();
    if file.cut {
        let mut args = FluentArgs::new();
        args.set("size", size_words(text.len() as u64));
        view.note = Some(tr_args("chat-file-cut", &args));
    }
    if file.language.as_deref() == Some("markdown") {
        view.kind = "markdown";
        view.blocks = blocks(&text);
    } else {
        view.kind = "code";
        view.lines = code_lines(&text, &file.spans);
    }
    view
}

/// `text` in lines, each in runs of one colour, as `spans` (byte ranges,
/// in order) colour it: an overlap or a range off a character's edge is
/// let go.
fn code_lines(text: &str, spans: &[(u32, u32, String)]) -> Vec<Vec<CodeRun>> {
    let mut runs: Vec<(usize, usize, Option<&str>)> = Vec::new();
    let mut at = 0;
    for (start, end, kind) in spans {
        let (start, end) = ((*start as usize).max(at), (*end as usize).min(text.len()));
        if end <= start || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }
        if start > at {
            runs.push((at, start, None));
        }
        runs.push((start, end, Some(kind.as_str())));
        at = end;
    }
    if at < text.len() {
        runs.push((at, text.len(), None));
    }
    let mut lines = vec![Vec::new()];
    for (start, end, kind) in runs {
        for (n, piece) in text[start..end].split('\n').enumerate() {
            if n > 0 {
                lines.push(Vec::new());
            }
            if !piece.is_empty() {
                lines.last_mut().expect("a line").push(CodeRun {
                    t: piece.trim_end_matches('\r').to_string(),
                    k: kind.map(str::to_string),
                });
            }
        }
    }
    // A file that ends its last line has no line after it.
    if text.ends_with('\n') && lines.len() > 1 && lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    lines
}

/// A file's size as a person reads it: "830 B", "12 KB", "1.4 MB".
fn size_words(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", (b / KB).max(1.0))
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / KB / KB)
    } else {
        format!("{:.1} GB", b / KB / KB / KB)
    }
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
            pictures: Vec::new(),
        }
    }

    fn read(target: &str) -> Look {
        Look {
            kind: LookKind::Read,
            target: target.into(),
            path: None,
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
                    pictures: Vec::new(),
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
                questions: Vec::new(),
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
                questions: Vec::new(),
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
                questions: Vec::new(),
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
            pictures: Vec::new(),
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
    fn a_worked_note_is_said_aside_with_when_it_was_done() {
        let asked = |id: &str| Item::User {
            id: id.into(),
            text: "go".into(),
            attachments: Vec::new(),
        };
        let said = |id: &str| Item::Text {
            id: id.into(),
            text: "done".into(),
        };
        let mut chat = Chat::new(1, "codex");
        chat.apply(Ev::History {
            items: vec![asked("u1"), said("a"), asked("u2"), said("b"), asked("u3")],
            more: false,
            reset: true,
        });
        chat.apply(Ev::Times {
            times: vec![
                ("u1".into(), 1000),
                ("a".into(), 1150),
                ("u2".into(), 2000),
                ("b".into(), 2010),
                ("u3".into(), 2100),
            ],
        });
        // From when it was asked; and, where that would put it past what
        // came next (words sent while it worked), when its reply came.
        chat.apply(Ev::Worked {
            id: "w1".into(),
            after: "a".into(),
            seconds: 169,
        });
        chat.apply(Ev::Worked {
            id: "w2".into(),
            after: "b".into(),
            seconds: 500,
        });
        let notes: Vec<(bool, Option<u64>)> = chat
            .view()
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Note { aside, done, .. } => Some((*aside, *done)),
                _ => None,
            })
            .collect();
        assert_eq!(notes, vec![(true, Some(1169)), (true, Some(2010))]);
    }

    #[test]
    fn a_load_the_bridge_makes_by_itself_says_what_it_waits_for_not_a_problem() {
        let mut chat = Chat::new(1, "claude");
        // An older bridge's card first, then the new one loading by itself.
        chat.line(r#"{"ev":"error","need":"pi_restart","message":"not running","can_load":true}"#);
        assert!(chat.view().problem.is_some());
        chat.line(r#"{"ev":"loading","wait":"turn"}"#);
        let view = chat.view();
        assert!(view.problem.is_none(), "waiting is no problem");
        assert_eq!(view.state, "connecting");
        let mut args = FluentArgs::new();
        args.set("agent", "Claude");
        assert_eq!(view.state_label, tr_args("chat-loading-after-turn", &args));
        chat.line(r#"{"ev":"loading","wait":"field"}"#);
        assert_eq!(chat.view().state_label, tr_args("chat-loading-after-field", &args));
        chat.line(r#"{"ev":"loading","wait":"typed"}"#);
        assert_eq!(chat.view().state_label, tr_args("chat-loading", &args));
        // A reason from a newer bridge is said as loading.
        chat.line(r#"{"ev":"loading","wait":"elsewhere"}"#);
        assert_eq!(chat.view().state_label, tr_args("chat-loading", &args));
        // Loaded: the chat as ever.
        chat.apply(Ev::Hello {
            protocol: 2,
            agent: "claude".into(),
            title: None,
            guessed: false,
        });
        assert_eq!(chat.view().state, "idle");
        // Given up: the card again, for the person.
        let mut chat = Chat::new(1, "claude");
        chat.line(r#"{"ev":"loading","wait":"typed"}"#);
        chat.line(r#"{"ev":"error","need":"pi_restart","message":"not running","can_load":true}"#);
        assert_eq!(chat.view().state, "problem");
    }

    #[test]
    fn what_is_said_shows_at_once_and_is_held_failed_or_sent_again() {
        let sending = |view: &ChatView| -> Vec<(String, Option<&'static str>)> {
            view.rows
                .iter()
                .filter_map(|row| match row {
                    Row::User { text, sending, .. } => Some((text.clone(), *sending)),
                    _ => None,
                })
                .collect()
        };
        let state_of = |chat: &Chat, text: &str| {
            // The last row with the words: what is on its way comes last.
            sending(&chat.view()).into_iter().rev().find(|(said, _)| said == text).map(|(_, state)| state)
        };
        let hello = |guessed: bool| Ev::Hello {
            protocol: 2,
            agent: "codex".into(),
            title: None,
            guessed,
        };
        let user = |id: &str, text: &str| Item::User {
            id: id.into(),
            text: text.into(),
            attachments: Vec::new(),
        };
        let history = |items: Vec<Item>| Ev::History { items, more: false, reset: true };
        let mut chat = Chat::new(1, "codex");
        // Said while it connects: on show at once, held, and sent once it is
        // greeted and has sent its history.
        let early = chat.sending("hi", &[]).unwrap();
        assert!(chat.view().can_send);
        assert_eq!(state_of(&chat, "hi"), Some(Some("sending")));
        assert!(chat.take_held().is_empty(), "not connected yet");
        chat.apply(hello(false));
        assert!(chat.take_held().is_empty(), "its history first");
        chat.apply(history(vec![user("u0", "yes")]));
        let held = chat.take_held();
        assert_eq!(held.iter().map(|(id, text, _)| (id.as_str(), text.as_str())).collect::<Vec<_>>(), [(early.as_str(), "hi")]);
        assert!(chat.take_held().is_empty(), "sent once");
        chat.sent_as(Some(early.clone()));
        // The agent takes it: one row, under the id it was shown with.
        chat.apply(Ev::Item { item: user("u1", "hi"), partial: false });
        let view = chat.view();
        assert_eq!(state_of(&chat, "hi"), Some(None));
        assert!(matches!(&view.rows[1], Row::User { id, .. } if *id == early));

        // Refused just after it went: not sent, and a tap sends it again.
        let refused = chat.sending("again", &[]).unwrap();
        chat.sent_as(Some(refused.clone()));
        chat.apply(Ev::Refused { message: "busy".into() });
        assert_eq!(state_of(&chat, "again"), Some(Some("failed")));
        assert_eq!(chat.resend(&refused).map(|(text, _)| text), Some("again".to_string()));
        assert_eq!(state_of(&chat, "again"), Some(Some("sending")));
        assert!(chat.resend(&refused).is_none(), "only what failed");
        // A refusal of something else marks nothing: another op, or the
        // older history asked for just before.
        chat.sent_as(None);
        chat.apply(Ev::Refused { message: "nothing is running".into() });
        chat.sent_as(Some(refused.clone()));
        chat.loading_older = true;
        chat.apply(Ev::Refused { message: "no older history".into() });
        assert_eq!(state_of(&chat, "again"), Some(Some("sending")));

        // A bridge that shows what was said at once, then refuses it: back
        // as not sent, the item it showed gone.
        let shown = chat.sending("look here", &[]).unwrap();
        chat.sent_as(Some(shown.clone()));
        chat.apply(Ev::Item { item: user("sent-1", "look here"), partial: false });
        assert_eq!(state_of(&chat, "look here"), Some(None));
        chat.apply(Ev::Refused { message: "dropped".into() });
        assert_eq!(state_of(&chat, "look here"), Some(Some("failed")));
        assert!(!chat.items.iter().any(|item| item.id() == "sent-1"));

        // Cut off: what went is not sure to have arrived; what is said
        // meanwhile is held, and goes once it is back.
        chat.lost();
        let cut = chat.sending("while cut off", &[]).unwrap();
        assert_eq!(state_of(&chat, "again"), Some(Some("failed")));
        assert_eq!(state_of(&chat, "while cut off"), Some(Some("sending")));
        chat.reconnecting();
        chat.apply(hello(false));
        // The history it sends again: an old "yes" takes nothing said now,
        // and a line typed in the terminal takes nothing held.
        let said_yes = chat.sending("yes", &[]).unwrap();
        chat.not_sent(&said_yes, "the connection dropped");
        chat.apply(history(vec![user("u0", "yes"), user("u1", "hi"), user("t1", "from the terminal")]));
        assert_eq!(state_of(&chat, "yes"), Some(Some("failed")), "the old yes is not it");
        assert_eq!(chat.take_held().iter().map(|(id, ..)| id.clone()).collect::<Vec<_>>(), [cut.clone()]);
        chat.sent_as(Some(cut));
        // What arrived after all is taken from the history, even failed.
        chat.reconnecting();
        chat.apply(hello(false));
        chat.apply(history(vec![user("u0", "yes"), user("u1", "hi"), user("u2", "again")]));
        assert_eq!(state_of(&chat, "again"), Some(None));
        // A resend while it is not connected is held again.
        chat.lost();
        assert!(chat.resend(&said_yes).is_none());
        assert_eq!(state_of(&chat, "yes"), Some(Some("sending")));
        // Greeted in a conversation picked among several: what is held is
        // not sent there by itself.
        chat.reconnecting();
        chat.apply(hello(true));
        chat.apply(history(Vec::new()));
        assert!(chat.take_held().is_empty());
        assert_eq!(state_of(&chat, "yes"), Some(Some("failed")));
        // It stopped for good: all of it is marked, held or not.
        let last = chat.sending("last", &[]).unwrap();
        chat.hold(&last);
        chat.ended(false, "the channel closed");
        assert_eq!(state_of(&chat, "last"), Some(Some("failed")));
        assert!(chat.take_held().is_empty());

        // A card the person can act on keeps what is held for after it.
        let mut chat = Chat::new(1, "claude");
        chat.sending("later", &[]).unwrap();
        chat.line(r#"{"ev":"error","need":"pi_restart","message":"not running","can_load":true}"#);
        assert_eq!(state_of(&chat, "later"), Some(Some("sending")));

        // A command shows no row; one typed while not connected goes back
        // to the field. A path or a dollar sign at the start is words.
        let mut chat = Chat::new(1, "codex");
        assert!(chat.sending("/compact", &[]).is_none());
        chat.command_not_sent("/compact");
        assert_eq!(chat.view().notice.and_then(|notice| notice.kept), Some("/compact".to_string()));
        assert!(chat.sending("/etc/hosts is wrong", &[]).is_some());
        assert!(chat.sending("$HOME is unset", &[]).is_some());
    }

    #[test]
    fn a_chat_cut_off_says_it_reconnects_and_keeps_what_it_held() {
        let mut chat = Chat::new(1, "pi");
        chat.apply(Ev::Hello {
            protocol: 2,
            agent: "pi".into(),
            title: None,
            guessed: false,
        });
        chat.apply(Ev::History {
            items: vec![Item::Text {
                id: "a".into(),
                text: "hi".into(),
            }],
            more: false,
            reset: true,
        });
        chat.lost();
        assert!(chat.reconnects());
        let view = chat.view();
        assert_eq!(view.state, "connecting");
        assert_eq!(view.state_label, tr("chat-reconnecting"));
        // What is said meanwhile is held, and goes once it is back.
        assert!(view.problem.is_none() && view.can_send);
        assert_eq!(view.rows.len(), 1, "what it held stays on show");
        // A channel opened anew still says so, until its bridge greets.
        chat.reconnecting();
        assert_eq!(chat.view().state_label, tr("chat-reconnecting"));
        chat.apply(Ev::Hello {
            protocol: 2,
            agent: "pi".into(),
            title: None,
            guessed: false,
        });
        assert_eq!(chat.view().state, "idle");
        assert!(!chat.reconnects());
        // Cut off for good: said so, with a way to try again.
        chat.ended(false, "gone");
        assert_eq!(chat.view().problem.and_then(|p| p.action).map(|a| a.id), Some("retry"));
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
                    description: String::new(),
                },
                ModelChoice {
                    id: "mini".into(),
                    name: String::new(),
                    efforts: vec![],
                    description: String::new(),
                },
            ],
        });
        let model = chat.view().model.unwrap();
        assert_eq!(model.label, format!("GPT-5.5 · {}", tr("chat-effort-high")));
        // An agent with a permission mode says it after them.
        chat.apply(Ev::Mode { mode: "auto".into() });
        assert_eq!(
            chat.view().model.unwrap().label,
            format!("GPT-5.5 · {} · {}", tr("chat-effort-high"), tr("chat-mode-auto"))
        );
        // Greeted again (the agent started again in the pane): its mode is
        // not known until it says it.
        chat.apply(Ev::Hello {
            agent: chat.agent.clone(),
            title: None,
            guessed: false,
            protocol: crate::PROTOCOL,
        });
        assert_eq!(chat.view().model.unwrap().label, format!("GPT-5.5 · {}", tr("chat-effort-high")));
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
                pictures: Vec::new(),
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

    fn picture(path: &str, thumb: Option<&str>) -> Attachment {
        Attachment {
            name: file_name(path),
            image: true,
            path: Some(path.into()),
            thumb: thumb.map(str::to_string),
        }
    }

    fn read_item(id: &str, path: &str, pictures: Vec<Attachment>) -> Item {
        Item::Command {
            id: id.into(),
            command: format!("read {path}"),
            status: Status::Done,
            output: String::new(),
            exit: None,
            looks: vec![Look {
                kind: LookKind::Read,
                target: path.into(),
                path: None,
            }],
            pictures,
        }
    }

    #[test]
    fn pictures_a_call_looked_at_show_under_its_line() {
        let mut chat = Chat::new(1, "claude");
        chat.apply(Ev::History {
            items: vec![
                read_item("r1", "shot.png", vec![picture("/w/shot.png", Some("AAAA"))]),
                read_item("r2", "src/main.rs", Vec::new()),
            ],
            more: false,
            reset: true,
        });
        let view = chat.view();
        let Row::Fold { icon: "look", pictures, .. } = &view.rows[0] else {
            panic!("not a look: {:?}", view.rows);
        };
        let shown: Vec<_> = pictures.iter().map(|p| (p.picture.clone(), p.path.clone())).collect();
        assert_eq!(shown, [(Some("r1#0".to_string()), Some("/w/shot.png".to_string()))]);
        assert_eq!(chat.new_pictures(), [("r1#0".to_string(), "AAAA".to_string())]);

        // Open, its lines say which file each opens: a read one, not a search.
        chat.toggle("r1");
        let view = chat.view();
        let Row::Fold { body: Some(FoldBody::List { lines, paths }), .. } = &view.rows[0] else {
            panic!("not open: {:?}", view.rows);
        };
        assert_eq!(lines, &["shot.png", "src/main.rs"]);
        assert_eq!(paths, &["shot.png", "src/main.rs"]);
        chat.toggle("r1");

        // A tool's picture (a screenshot), and a group of several things
        // done, which shows its folds' pictures while closed.
        chat.apply(Ev::item(Item::Tool {
            id: "t1".into(),
            name: "browser.screenshot".into(),
            detail: String::new(),
            status: Status::Done,
            pictures: vec![picture("/s/a.png", Some("BBBB")), picture("/s/b.png", None)],
        }));
        let view = chat.view();
        let Row::Group { open: false, pictures, .. } = &view.rows[0] else {
            panic!("not a group: {:?}", view.rows);
        };
        let keys: Vec<_> = pictures.iter().map(|p| p.picture.clone()).collect();
        assert_eq!(keys, [Some("r1#0".to_string()), Some("t1#0".to_string()), None]);
        assert_eq!(chat.new_pictures(), [("t1#0".to_string(), "BBBB".to_string())]);
    }

    #[test]
    fn a_file_shown_is_laid_out_once() {
        let mut chat = Chat::new(1, "codex");
        let text = "fn main() {\n    \"hi\"\n}\n";
        chat.apply(Ev::File {
            file: FileShown {
                path: "src/main.rs".into(),
                size: 2048,
                text: Some(text.into()),
                language: Some("rust".into()),
                spans: vec![(0, 2, "keyword".into()), (3, 7, "function".into()), (16, 20, "string".into())],
                ..FileShown::default()
            },
        });
        let file = chat.take_files().pop().expect("the file");
        assert!(chat.take_files().is_empty(), "given once");
        assert_eq!((file.name.as_str(), file.size.as_str(), file.kind), ("main.rs", "2 KB", "code"));
        let lines: Vec<Vec<(String, Option<String>)>> =
            file.lines.into_iter().map(|line| line.into_iter().map(|run| (run.t, run.k)).collect()).collect();
        let run = |t: &str, k: Option<&str>| (t.to_string(), k.map(str::to_string));
        assert_eq!(
            lines,
            [
                vec![run("fn", Some("keyword")), run(" ", None), run("main", Some("function")), run("() {", None)],
                vec![run("    ", None), run("\"hi\"", Some("string"))],
                vec![run("}", None)],
            ]
        );
        assert_eq!(file.note, None);

        // Markdown is laid out; a long one says only its start is shown.
        chat.apply(Ev::File {
            file: FileShown {
                path: "README.md".into(),
                size: 900_000,
                text: Some("# Title\n\nWords.".into()),
                cut: true,
                language: Some("markdown".into()),
                ..FileShown::default()
            },
        });
        let file = chat.take_files().pop().unwrap();
        assert_eq!(file.kind, "markdown");
        assert_eq!(file.blocks.len(), 2);
        assert!(file.note.is_some());

        // A picture as it is; one that could not be read, why not.
        chat.apply(Ev::File {
            file: FileShown {
                path: "/w/shot.png".into(),
                picture: Some("CCCC".into()),
                ..FileShown::default()
            },
        });
        let file = chat.take_files().pop().unwrap();
        assert_eq!((file.kind, file.picture.as_deref()), ("picture", Some("CCCC")));
        chat.apply(Ev::File {
            file: FileShown {
                path: "gone.txt".into(),
                error: Some("missing".into()),
                ..FileShown::default()
            },
        });
        let file = chat.take_files().pop().unwrap();
        assert_eq!((file.kind, file.note), ("none", Some(tr("chat-file-missing"))));

        // Spans that overlap or cut a character are let go, not trusted.
        let lines = code_lines("é = 1", &[(0, 1, "variable".into()), (0, 2, "keyword".into()), (5, 6, "number".into())]);
        assert_eq!(
            lines,
            [vec![
                CodeRun { t: "é".into(), k: Some("keyword".into()) },
                CodeRun { t: " = ".into(), k: None },
                CodeRun { t: "1".into(), k: Some("number".into()) },
            ]]
        );
        assert_eq!(size_words(830), "830 B");
        assert_eq!(size_words(3 << 20), "3.0 MB");
    }

    #[test]
    fn models_say_what_they_are_for() {
        let mut chat = Chat::new(1, "codex");
        chat.apply(Ev::Models {
            current: Some("gpt-5.5".into()),
            effort: None,
            models: vec![ModelChoice {
                id: "gpt-5.5".into(),
                name: "GPT-5.5".into(),
                efforts: Vec::new(),
                description: "Frontier model for coding".into(),
            }],
        });
        let model = chat.view().model.unwrap();
        assert_eq!(model.models[0].description, "Frontier model for coding");
    }
}
