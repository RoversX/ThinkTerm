//! A coding agent's conversation as a chat.
//!
//! A client (the phone) runs `thinkterm cli agent chat --pane N` on the
//! machine the pane lives on, on a channel of its own beside the mux
//! protocol, and the two exchange one JSON object per line: [`Op`]s from
//! the client, [`Ev`]s from the bridge. The bridge speaks each agent's own
//! interface -- Codex's app-server daemon, the extension ThinkTerm puts
//! into Pi, OpenCode's background service -- and tells what happens in
//! the shapes below (Claude Code too, through the mod ThinkTerm puts into
//! it), the same for
//! every agent. Nothing here is worded for a person: the client words it,
//! in the language it shows.

use serde::{Deserialize, Serialize};

pub mod diff;
#[cfg(all(feature = "host", unix))]
pub mod host;
#[cfg(feature = "view")]
pub mod markdown;
#[cfg(feature = "view")]
pub mod view;

/// The command that has a running agent take ThinkTerm's extension in
/// anew: Pi's `/reload`; OMP's reloads only its plugins, and its
/// `/restart` the extensions too; Claude Code's `/reload-plugins`.
pub fn reload_command(agent_id: &str) -> &'static str {
    match agent_id {
        "omp" => "/restart",
        "claude" => "/reload-plugins",
        _ => "/reload",
    }
}

/// Whether a chat can be had with an agent, by the id its detection gives
/// it (`thinkterm_proto::AgentStatus::agent_id`).
pub fn chat_agent(agent_id: &str) -> bool {
    matches!(agent_id, "codex" | "pi" | "omp" | "opencode" | "claude")
}

/// Bumped when a line changes meaning. Either side ignores the fields it
/// does not know. 2: what is said may carry attachments, and the model
/// can be changed ([`Op::SetModel`], [`Ev::Models`]). 3: the agent says
/// what commands it takes ([`Ev::Commands`]), sent as what is said, and
/// how full its context is ([`Ev::Context`]). Added since, without a
/// bump (an older side lets them go): the pictures a call looked at
/// ([`Item::Command`], [`Item::Tool`]), a file shown on the client's
/// asking ([`Op::Open`], [`Ev::File`]), what a model is for
/// ([`ModelChoice::description`]).
pub const PROTOCOL: u32 = 3;
/// The longest line either side takes in.
pub const MAX_LINE: usize = 8 << 20;

/// What a client asks of the bridge.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Say something to the agent: a turn of its own when the agent
    /// rests, added to the running one when it works. Pictures go along
    /// as pictures; other files, by their paths.
    Send {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    /// Stop the running turn.
    Interrupt,
    /// Answer what the agent asked ([`Ask::id`]) with one of its
    /// [`Ask::choices`]. A question ([`AskKind::Question`]) is answered
    /// with `accept` and `answers`: what was picked for each of its
    /// [`Ask::questions`], in order (several picks joined by ", ").
    Answer {
        ask: String,
        choice: Choice,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        answers: Vec<String>,
    },
    /// History from before the oldest item sent so far.
    Older,
    /// Put ThinkTerm's extension into the agent, after an
    /// [`Ev::Error`] said one is needed ([`Need::PiExtension`]).
    Install,
    /// Have the agent running in the pane take in its extensions anew
    /// (Pi's `/reload`), as the bridge offered: an [`Ev::Error`] that
    /// `can_load`, or [`Ev::Outdated`].
    Reload,
    /// Talk to another model ([`ModelChoice::id`]), or think harder or
    /// less (one of its [`ModelChoice::efforts`]): from the next request
    /// on (Pi), or the next turn (Codex).
    SetModel {
        model: String,
        #[serde(default)]
        effort: Option<String>,
    },
    /// Show the file at `path`, one the conversation named (sent, read,
    /// changed, a picture it looked at): absolute, or from where the agent
    /// was started. Answered by [`Ev::File`], by the bridge itself.
    Open { path: String },
}

/// What the bridge says.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Ev {
    /// The first line once the pane's conversation is found.
    Hello {
        protocol: u32,
        /// `codex`, `pi`, `omp`, `opencode`, `claude`.
        agent: String,
        /// The conversation's own name, when it has one.
        #[serde(default)]
        title: Option<String>,
        /// Picked among several that could each be the pane's: the client
        /// says so, since it may be the wrong one.
        #[serde(default)]
        guessed: bool,
    },
    /// Items, oldest first. With `reset` they replace all the client
    /// holds; without, they go before the oldest it holds (`Op::Older`).
    History {
        items: Vec<Item>,
        /// There is more before these.
        #[serde(default)]
        more: bool,
        #[serde(default)]
        reset: bool,
    },
    /// An item new or changed: it replaces the one with its id, or goes
    /// after all the others.
    Item {
        item: Item,
        /// Still being written: more of it comes, until it is sent without.
        #[serde(default, skip_serializing_if = "is_false")]
        partial: bool,
    },
    /// What the agent is doing, since when (unix seconds).
    State {
        state: AgentState,
        #[serde(default)]
        since: Option<u64>,
    },
    /// The agent waits for an answer.
    Ask { ask: Ask },
    /// What can be typed for the agent to do rather than read: `/compact`,
    /// a prompt template, a skill (`$review`). Sent as what is said
    /// ([`Op::Send`]); the bridge runs the ones the agent only takes in its
    /// terminal.
    Commands { commands: Vec<CommandInfo> },
    /// The model the agent talks to, how hard it thinks, and the others
    /// it could talk to.
    Models {
        #[serde(default)]
        current: Option<String>,
        #[serde(default)]
        effort: Option<String>,
        #[serde(default)]
        models: Vec<ModelChoice>,
    },
    /// How full the model's context is, and what fills it.
    Context { context: ContextInfo },
    /// What was asked was answered, here or anywhere else.
    Resolved { id: String },
    /// Something done that the client may want to tell (installed, sent).
    Done { what: Done },
    /// What the client asked could not be done; the chat goes on.
    /// `message` is the agent's own word on why, in English.
    Refused { message: String },
    /// The bridge cannot go on, or did not get started. `need` says what
    /// would help; `message` is the detail, in English, for a person who
    /// wants it.
    Error {
        #[serde(default)]
        need: Option<Need>,
        message: String,
        /// The extension is in place but not running in this agent, and
        /// the bridge can have the agent take it in ([`Op::Reload`]).
        #[serde(default, skip_serializing_if = "is_false")]
        can_load: bool,
    },
    /// The agent runs an older ThinkTerm extension than the one now in
    /// its folder: the chat goes on, without what is newer, until the
    /// agent takes it in ([`Op::Reload`]).
    Outdated,
    /// The agent runs no ThinkTerm extension yet, and the bridge is having
    /// it take one in by itself: what that waits for, said while the chat
    /// connects -- no problem to put right. A line of its own, as
    /// [`Ev::Worked`] is: a client that does not know it shows the chat
    /// connecting.
    Loading { wait: LoadWait },
    /// How long a turn worked, said after the last thing that came of it
    /// (the item `after`), where the client shows it. A line of its own,
    /// not an item: a client that does not know it lets it go, where an
    /// item it did not know would cost it the whole history it came in.
    Worked { id: String, after: String, seconds: u64 },
    /// When things said and done came about (unix seconds), by item id:
    /// said after history, for what it holds. A line of its own, as
    /// [`Ev::Worked`] is.
    Times { times: Vec<(String, u64)> },
    /// How the agent takes its permission prompts, in its own word for it
    /// (Claude Code's `default`, `acceptEdits`, `plan`, `auto`): said by a
    /// bridge whose agent has such a mode, when it is known or changes.
    Mode { mode: String },
    /// A file, as [`Op::Open`] asked for it.
    File { file: FileShown },
}

/// A file as a client shows it: its text (a code file's colours told),
/// or a picture of it, or why neither.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileShown {
    /// As [`Op::Open`] asked for it.
    pub path: String,
    /// How large it is, in bytes.
    #[serde(default)]
    pub size: u64,
    /// What it holds, when that is text: the start of it, at most
    /// [`FILE_TEXT_SHOWN`] bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// There is more of the text than is shown.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cut: bool,
    /// The language its text is in, as the highlighter names it (`rust`,
    /// `markdown`); none when not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// How the text is coloured: byte ranges of it and what each is
    /// (`keyword`, `string`, `comment`, `function`, ...), in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spans: Vec<(u32, u32, String)>,
    /// A picture of it (JPEG, base64), when it is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    /// Why it is not shown: `missing`, `folder`, `binary`, `too_big`,
    /// `unreadable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The most of a file's text [`Ev::File`] carries.
pub const FILE_TEXT_SHOWN: usize = 256 << 10;

impl Ev {
    /// An item, whole.
    pub fn item(item: Item) -> Self {
        Ev::Item { item, partial: false }
    }

    /// An item as far as it is written: more of it comes.
    pub fn partial(item: Item) -> Self {
        Ev::Item { item, partial: true }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One thing in the conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    /// What the person said, and what they sent with it.
    User {
        id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    /// What the agent said, in Markdown.
    Text { id: String, text: String },
    /// What the agent thought, or the summary of it it gives.
    Thinking { id: String, text: String },
    /// A command it ran.
    Command {
        id: String,
        command: String,
        status: Status,
        /// The end of what it printed.
        #[serde(default)]
        output: String,
        #[serde(default)]
        exit: Option<i32>,
        /// What it looked at, when all it did was look (read a file,
        /// search, list a folder): such runs fold together.
        #[serde(default)]
        looks: Vec<Look>,
        /// The pictures it looked at (a picture file it read), each with
        /// where it is, so that it can be opened.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pictures: Vec<Attachment>,
    },
    /// Files it changed.
    Edit {
        id: String,
        status: Status,
        files: Vec<FileEdit>,
    },
    /// Another tool it used (an MCP tool, a web search, a sub-agent).
    Tool {
        id: String,
        name: String,
        #[serde(default)]
        detail: String,
        status: Status,
        /// The pictures that came of it (a picture looked at, a screenshot
        /// taken), as [`Item::Command::pictures`].
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pictures: Vec<Attachment>,
    },
    /// Something that happened to the conversation itself.
    Note {
        id: String,
        note: NoteKind,
        #[serde(default)]
        text: String,
    },
}

impl Item {
    pub fn id(&self) -> &str {
        match self {
            Item::User { id, .. }
            | Item::Text { id, .. }
            | Item::Thinking { id, .. }
            | Item::Command { id, .. }
            | Item::Edit { id, .. }
            | Item::Tool { id, .. }
            | Item::Note { id, .. } => id,
        }
    }
}

/// A file sent along with what was said, put on the agent's machine
/// first (a phone's go to `~/ThinkTerm_Uploads`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Its name, as the person knows it.
    #[serde(default)]
    pub name: String,
    /// A picture, which the agent is shown as one; else a file whose path
    /// it is told.
    #[serde(default)]
    pub image: bool,
    /// Where it is on the agent's machine, when it is a file there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A small picture of it (JPEG, base64), made by the bridge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumb: Option<String>,
}

/// How full the model's context is: what the conversation takes of its
/// window, what that is made of, and what the session has used so far.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextInfo {
    /// Tokens the conversation takes of the window now; none while not
    /// known (before the first answer, just after a compaction).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<u64>,
    /// The model's window, in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u64>,
    /// What fills it, in tokens: estimated from what it holds, and adding
    /// up to `used` where that is known.
    #[serde(default)]
    pub parts: Vec<ContextPart>,
    /// Where the agent sums up older turns by itself, in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacts_at: Option<u64>,
    /// The session's tokens so far, every turn's.
    #[serde(default)]
    pub totals: Usage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPart {
    pub kind: PartKind,
    pub tokens: u64,
}

/// What a part of the context is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartKind {
    /// The agent's own instructions.
    System,
    /// What the tools it may call are and take.
    Tools,
    /// The project's instructions (`AGENTS.md`) and what it is told of
    /// where it runs.
    Instructions,
    /// What the person said.
    User,
    /// What the agent said.
    Assistant,
    Thinking,
    /// Its tool calls and what came of them.
    ToolUse,
    /// Earlier turns, summed up.
    Summary,
    #[serde(other)]
    Other,
}

/// Tokens used, every turn's together; each none when the agent does not
/// say.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    /// What it cost, in US dollars, as the agent reckons it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

/// A command the agent takes, as typed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInfo {
    /// As typed, its sign included: `/compact`, `$skill`.
    pub name: String,
    /// What it does, in the agent's own words.
    #[serde(default)]
    pub description: String,
    /// What follows it ("<name>"); none for one that runs as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    /// What it is; none from a bridge that does not say, when its name
    /// tells (`$skill`, `/skill:name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<CommandKind>,
}

/// What a command is, as the field groups them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    /// Something the agent does: its own, or an extension's.
    Command,
    /// Instructions it takes up for what follows.
    Skill,
    /// Words written ahead, sent in the command's place.
    Prompt,
    #[serde(other)]
    Other,
}

/// A model the agent can be switched to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    /// What [`Op::SetModel`] names it by.
    pub id: String,
    /// As people know it ("GPT-5.5").
    #[serde(default)]
    pub name: String,
    /// How hard it can think, in the agent's own words ("low", "high");
    /// none when it cannot be told.
    #[serde(default)]
    pub efforts: Vec<String>,
    /// What it is for, in the agent's own words, when it says.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Done,
    Failed,
    /// Turned down when it was asked about.
    Declined,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Look {
    pub kind: LookKind,
    /// The file or folder, or what was searched for.
    pub target: String,
    /// Where the file read is, when `target` names it only as it is shown
    /// (its name alone): what opens it ([`Op::Open`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LookKind {
    Read,
    Search,
    List,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEdit {
    pub path: String,
    pub added: u32,
    pub removed: u32,
    /// A unified diff, cut short when long.
    #[serde(default)]
    pub diff: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    /// Earlier turns were summed up to make room.
    Compacted,
    /// The turn was stopped.
    Interrupted,
    /// The turn failed; `text` says how.
    Failed,
    /// How long the turn worked: `text` is its seconds. Made by the view
    /// from [`Ev::Worked`]; never sent as an item.
    Worked,
    /// A kind this side does not know yet: shown as nothing.
    #[serde(other)]
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Working,
    Idle,
    /// Waiting for a person's answer.
    Blocked,
}

/// A question the agent waits on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    pub id: String,
    pub kind: AskKind,
    /// The command it would run, or the files it would change.
    pub body: String,
    /// Why, in the agent's words.
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// What it may be answered with here. None: the agent takes its answer
    /// only in its terminal (OMP's approvals), and the client points there.
    #[serde(default)]
    pub choices: Vec<Choice>,
    /// What a [`AskKind::Question`] asks, one or more questions at once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<Question>,
}

/// A question the agent puts to the person, with the answers it offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    /// A word or two for it ("Library").
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    /// Several may be picked.
    #[serde(default)]
    pub multi: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    /// May it run a command?
    Command,
    /// May it change files?
    Edit,
    /// May it use another tool (`body` names it)?
    Tool,
    /// It asks the person something ([`Ask::questions`]).
    Question,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    Accept,
    /// Yes, and the same again without asking, for this session.
    AcceptForSession,
    Decline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Done {
    /// The extension is in place; the agent takes it in when it next
    /// starts or reloads.
    Installed,
}

/// What a load the bridge makes by itself ([`Ev::Loading`]) waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadWait {
    /// Typed into the agent: it is taking the extension in.
    Typed,
    /// The agent is in a turn: it is typed once that ends.
    Turn,
    /// Something is typed in the agent's field, which is left as it is:
    /// it is typed once the field is empty.
    Field,
    /// The agent's field is not on its screen (a dialog, a menu): it is
    /// typed once the field is back.
    Screen,
    /// A reason this client does not know.
    #[serde(other)]
    Other,
}

/// What would let the chat work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Need {
    /// Pi has no ThinkTerm extension yet: `Op::Install` puts it in.
    PiExtension,
    /// The extension is in place but this Pi started without it.
    PiRestart,
    /// No Codex app-server to talk to (an older Codex, or none running).
    CodexDaemon,
    /// The daemon has no conversation that is this pane's.
    CodexThread,
    /// No OpenCode service answers: none runs, or the TUI runs a private
    /// one where it cannot be read.
    OpenCodeService,
    /// The service has no session that is this pane's.
    OpenCodeSession,
    /// The pane runs OpenCode 1, which has no service to talk to.
    OpenCodeUpgrade,
    /// The pane is not on the machine the bridge runs on.
    NotHere,
    /// The pane runs no agent the chat knows.
    Unsupported,
    /// The machine's ThinkTerm has no bridge (an older one). Said by the
    /// client, which finds out when the command is missing.
    Bridge,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip() {
        let ev = Ev::item(Item::Command {
            id: "c1".into(),
            command: "cargo test".into(),
            status: Status::Done,
            output: "ok".into(),
            exit: Some(0),
            looks: vec![],
            pictures: Vec::new(),
        });
        let line = serde_json::to_string(&ev).unwrap();
        assert!(line.contains(r#""ev":"item""#) && line.contains(r#""kind":"command""#));
        assert_eq!(serde_json::from_str::<Ev>(&line).unwrap(), ev);

        let op: Op = serde_json::from_str(r#"{"op":"answer","ask":"7","choice":"accept_for_session"}"#).unwrap();
        assert_eq!(
            op,
            Op::Answer {
                ask: "7".into(),
                choice: Choice::AcceptForSession,
                answers: Vec::new(),
            }
        );
    }

    #[test]
    fn fields_a_side_does_not_know_are_ignored() {
        let ev: Ev =
            serde_json::from_str(r#"{"ev":"hello","protocol":1,"agent":"pi","later":true}"#).unwrap();
        assert!(matches!(ev, Ev::Hello { guessed: false, title: None, .. }));
    }
}
