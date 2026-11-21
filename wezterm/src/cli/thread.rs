//! `thinkterm cli thread`: the threads the sidebar shows, as the server that
//! keeps them has them, and new ones made there.
//!
//! A thread is a row of the server's ThinkTerm tree (Space -> Project ->
//! Thread) that names a mux workspace. Windows in a workspace no thread
//! names exist on the server but appear in no sidebar, so a program that
//! starts work for a person to see has to put it in a thread.
use crate::cli::{printable, CliOutputFormatKind, NoSuchTarget, ServerUnreachable, UsageError};
use anyhow::Context;
use clap::Parser;
use codec::{MutateThinkTermTree, ThinkTermTree, TreeOp, TtProject, TtThread};
use std::io::Write;
use std::path::{Path, PathBuf};
use tabout::{tabulate_output, tabulate_output_as_string, Alignment, Column};
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct ThreadCommand {
    #[command(subcommand)]
    sub: ThreadSubCommand,
}

#[derive(Debug, Parser, Clone)]
enum ThreadSubCommand {
    /// List the threads of the projects that are not archived: their Space,
    /// project, name, id and workspace.
    #[command(name = "list")]
    List(ListThreads),

    /// Make a thread in a project and print its id. It shows in the sidebar
    /// at once, and its terminal starts when it is first opened there, or
    /// when `thinkterm cli spawn --thread` opens it to put a program in it.
    #[command(name = "create")]
    Create(CreateThread),
}

#[derive(Debug, Parser, Clone)]
struct ListThreads {
    /// Only the threads of this project: its name or its id.
    #[arg(long)]
    project: Option<String>,

    /// Controls the output format.
    /// "table" and "json" are possible formats.
    #[arg(long = "format", default_value = "table")]
    format: CliOutputFormatKind,

    /// The same as `--format json`.
    #[arg(long, conflicts_with = "format")]
    json: bool,
}

#[derive(Debug, Parser, Clone)]
struct CreateThread {
    /// The thread's name. A project that has a thread of that name already
    /// gets this one numbered, as the sidebar numbers them.
    #[arg(long)]
    name: String,

    /// The project to make it in: its name or its id. The default is the
    /// project of the thread the current pane ($WEZTERM_PANE) is in.
    #[arg(long)]
    project: Option<String>,

    /// Print the new thread as JSON (id, name, project and workspace)
    /// rather than its id alone.
    #[arg(long)]
    json: bool,
}

impl ThreadCommand {
    pub async fn run(self, client: Client) -> anyhow::Result<()> {
        let server = thread_server(client).await?;
        let tree = server.server.get_thinkterm_tree().await?.tree;
        match self.sub {
            ThreadSubCommand::List(cmd) => cmd.run(&server, &tree),
            ThreadSubCommand::Create(cmd) => cmd.run(&server, &tree).await,
        }
    }
}

#[derive(serde::Serialize)]
struct ListedThread<'a> {
    space: &'a str,
    space_id: &'a str,
    project: &'a str,
    project_id: &'a str,
    thread: &'a str,
    thread_id: &'a str,
    workspace: String,
    pinned: bool,
}

impl ListThreads {
    fn run(&self, server: &ThreadServer, tree: &ThinkTermTree) -> anyhow::Result<()> {
        let only = match &self.project {
            Some(spec) => Some(
                find_project(tree, spec, caller_space(tree, &server.caller))?
                    .id
                    .as_str(),
            ),
            None => None,
        };
        let listed: Vec<ListedThread> = tree
            .projects
            .iter()
            .filter(|project| project.archived_at.is_none())
            .filter(|project| only.map_or(true, |id| project.id == id))
            .flat_map(|project| {
                let space = tree.space(&project.space_id);
                project.threads.iter().map(move |thread| ListedThread {
                    space: space.map_or("", |space| space.name.as_str()),
                    space_id: project.space_id.as_str(),
                    project: project.name.as_str(),
                    project_id: project.id.as_str(),
                    thread: thread.name.as_str(),
                    thread_id: thread.id.as_str(),
                    workspace: thread_workspace(thread),
                    pinned: thread.is_pinned,
                })
            })
            .collect();
        let format = if self.json {
            CliOutputFormatKind::Json
        } else {
            self.format
        };
        match format {
            CliOutputFormatKind::Json => crate::cli::print_json(&listed),
            CliOutputFormatKind::Table => {
                let columns =
                    ["SPACE", "PROJECT", "THREAD", "ID", "WORKSPACE"].map(|name| Column {
                        name: name.to_string(),
                        alignment: Alignment::Left,
                    });
                // Every cell as it can be shown: names and ids come from a
                // server, which takes what any client sends it.
                let rows: Vec<Vec<String>> = listed
                    .iter()
                    .map(|row| {
                        vec![
                            printable(row.space),
                            printable(row.project),
                            printable(row.thread),
                            printable(row.thread_id),
                            printable(&row.workspace),
                        ]
                    })
                    .collect();
                let mut out = vec![];
                tabulate_output(&columns, &rows, &mut out)?;
                print_out(&out)
            }
        }
    }
}

impl CreateThread {
    async fn run(&self, server: &ThreadServer, tree: &ThinkTermTree) -> anyhow::Result<()> {
        let project = match &self.project {
            Some(spec) => find_project(tree, spec, caller_space(tree, &server.caller))?,
            None => current_project(tree, &server.caller)
                .ok_or_else(|| no_project_here(server, tree))?,
        };
        let (thread, _) = create_thread(&server.server, project, &self.name).await?;
        if self.json {
            #[derive(serde::Serialize)]
            struct Created<'a> {
                thread_id: &'a str,
                name: &'a str,
                project_id: &'a str,
                workspace: String,
            }
            crate::cli::print_json(&Created {
                thread_id: &thread.id,
                name: &thread.name,
                project_id: &thread.project_id,
                workspace: thread_workspace(&thread),
            })
        } else {
            print_out(format!("{}\n", printable(&thread.id)).as_bytes())
        }
    }
}

/// `bytes` on stdout; a reader that went away ends the command quietly
/// (`crate::stdout_error`), as with every command's output.
pub(crate) fn print_out(bytes: &[u8]) -> anyhow::Result<()> {
    let out = std::io::stdout();
    let mut out = out.lock();
    out.write_all(bytes).map_err(crate::stdout_error)?;
    out.flush().map_err(crate::stdout_error)
}

/// The workspace `thread` opens: the one it was created with, else the one
/// reserved for it, else the name the server gives a thread of its project
/// -- the order the sidebar takes them in (`activation_plan_for_thread`).
pub(crate) fn thread_workspace(thread: &TtThread) -> String {
    thread
        .materialized_workspace_name
        .clone()
        .or_else(|| thread.planned_workspace_name.clone())
        .unwrap_or_else(|| workspace_name(&thread.project_id, &thread.id))
}

/// The server's name for the workspace of thread `thread_id` of project
/// `project_id` (`thinkterm_tree::workspace_name` on the server).
fn workspace_name(project_id: &str, thread_id: &str) -> String {
    format!("thinkterm:{project_id}:{thread_id}")
}

/// The thread whose workspace `workspace` is -- the one it opens, which is
/// the one the sidebar shows for it -- and its project, if any thread's is.
pub(crate) fn workspace_owner<'a>(
    tree: &'a ThinkTermTree,
    workspace: &str,
) -> Option<(&'a TtProject, &'a TtThread)> {
    tree.projects.iter().find_map(|project| {
        project
            .threads
            .iter()
            .find(|thread| thread_workspace(thread) == workspace)
            .map(|thread| (project, thread))
    })
}

/// Whether `workspace` is shaped like the workspace of a thread
/// (`thinkterm:PROJECT:THREAD`), so that one no thread has is a mistake
/// rather than a plain workspace of the user's own.
pub(crate) fn names_a_thread(workspace: &str) -> bool {
    workspace.starts_with("thinkterm:")
}

/// The directory a program put in a thread of `project` starts in, as the
/// server starts a thread's own shell (`spawn_default_thinkterm_thread`):
/// the project's path, unless it has none or is the server's own row. A `~`
/// in it is the server's to expand, against its own home.
pub(crate) fn project_dir(project: &TtProject) -> Option<String> {
    match project.path.trim() {
        "" => None,
        path if path.starts_with("wezterm-mux://") => None,
        path => Some(path.to_string()),
    }
}

/// The name of the Space `project` is in, or nothing.
fn space_name<'a>(tree: &'a ThinkTermTree, project: &TtProject) -> &'a str {
    tree.space(&project.space_id)
        .map_or("", |space| space.name.as_str())
}

/// The live project `spec` names: its id, or else its name. A name several
/// live projects have is the one of them in `near` -- the Space of the
/// caller's own thread -- when just one is there; otherwise they are listed
/// and none is picked.
pub(crate) fn find_project<'a>(
    tree: &'a ThinkTermTree,
    spec: &str,
    near: Option<&str>,
) -> anyhow::Result<&'a TtProject> {
    let spec = spec.trim();
    if let Some(project) = tree.project(spec) {
        return live(project);
    }
    let named: Vec<&TtProject> = tree
        .projects
        .iter()
        .filter(|project| project.name == spec && project.archived_at.is_none())
        .collect();
    match named.as_slice() {
        [project] => Ok(project),
        [] => Err(NoSuchTarget(format!(
            "there is no project {}; `thinkterm cli thread list` shows them",
            quoted(spec)
        ))
        .into()),
        several => {
            let mine: Vec<&TtProject> = several
                .iter()
                .copied()
                .filter(|project| Some(project.space_id.as_str()) == near)
                .collect();
            if let [project] = mine.as_slice() {
                return Ok(project);
            }
            Err(NoSuchTarget(format!(
                "several projects are called {}; name one by its id:\n{}",
                quoted(spec),
                projects_table(tree, several)
            ))
            .into())
        }
    }
}

pub(crate) fn live(project: &TtProject) -> anyhow::Result<&TtProject> {
    if project.archived_at.is_some() {
        return Err(UsageError(format!(
            "the project {} is archived; restore it in the sidebar first",
            quoted(&project.name)
        ))
        .into());
    }
    Ok(project)
}

/// Where a thread's name is looked for.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Scope<'a> {
    /// The project `--project` names: there and nowhere else.
    Given(&'a TtProject),
    /// The project of the caller's own thread: there first. A name only
    /// other projects' threads have is not taken for one of theirs.
    Caller(&'a TtProject),
    /// No project to start from: every live project.
    Anywhere,
}

impl<'a> Scope<'a> {
    /// The project a thread of a name none has is made in.
    pub(crate) fn project(self) -> Option<&'a TtProject> {
        match self {
            Scope::Given(project) | Scope::Caller(project) => Some(project),
            Scope::Anywhere => None,
        }
    }
}

/// The thread `spec` names: its id anywhere, or else its name, looked for
/// as `scope` says. `None` when no thread is to be found and one of that
/// name may be made. When the name is several threads', or only other
/// projects' than the caller's, they are listed -- Space, project, thread
/// and id -- and none is picked.
pub(crate) fn find_thread<'a>(
    tree: &'a ThinkTermTree,
    spec: &str,
    scope: Scope,
) -> anyhow::Result<Option<&'a TtThread>> {
    let spec = spec.trim();
    if let Some(thread) = tree.thread(spec) {
        if let Some(owner) = tree.project_of_thread(&thread.id) {
            live(owner)?;
            if let Scope::Given(project) = scope {
                if project.id != owner.id {
                    return Err(UsageError(format!(
                        "the thread {} is in the project {}, not {}",
                        quoted(&thread.name),
                        quoted(&owner.name),
                        quoted(&project.name)
                    ))
                    .into());
                }
            }
        }
        return Ok(Some(thread));
    }
    let called = |only: Option<&TtProject>| -> Vec<(&'a TtProject, &'a TtThread)> {
        tree.projects
            .iter()
            .filter(|candidate| candidate.archived_at.is_none())
            .filter(|candidate| only.map_or(true, |only| candidate.id == only.id))
            .flat_map(|candidate| {
                candidate
                    .threads
                    .iter()
                    .filter(|thread| thread.name == spec)
                    .map(move |thread| (candidate, thread))
            })
            .collect()
    };
    let several = |threads: &[(&TtProject, &TtThread)]| -> anyhow::Error {
        NoSuchTarget(format!(
            "several threads are called {}; name one by its id, or its project with --project:\n{}",
            quoted(spec),
            threads_table(tree, threads)
        ))
        .into()
    };
    let (looked_in, found) = match scope {
        Scope::Given(project) | Scope::Caller(project) => (Some(project), called(Some(project))),
        Scope::Anywhere => (None, called(None)),
    };
    match found.as_slice() {
        [(_, thread)] => return Ok(Some(thread)),
        [] => {}
        threads => return Err(several(threads)),
    }
    if let (Scope::Caller(_), Some(mine)) = (scope, looked_in) {
        let elsewhere = called(None);
        if !elsewhere.is_empty() {
            return Err(NoSuchTarget(format!(
                "no thread of the project {} is called {}, but threads of other projects \
                 are; name one of them by its id, or make one in {} with --project {}:\n{}",
                quoted(&mine.name),
                quoted(spec),
                quoted(&mine.name),
                printable(&mine.id),
                threads_table(tree, &elsewhere)
            ))
            .into());
        }
    }
    Ok(None)
}

/// `rows` under `columns`, each line indented, for a message that lists
/// them. Every cell as it can be shown.
fn indented_table(columns: &[&str], rows: Vec<Vec<String>>) -> String {
    let columns: Vec<Column> = columns
        .iter()
        .map(|name| Column {
            name: name.to_string(),
            alignment: Alignment::Left,
        })
        .collect();
    let rows: Vec<Vec<String>> = rows
        .into_iter()
        .map(|row| row.iter().map(|cell| printable(cell)).collect())
        .collect();
    tabulate_output_as_string(&columns, &rows)
        .unwrap_or_default()
        .lines()
        .map(|line| format!("  {}", line.trim_end()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn threads_table(tree: &ThinkTermTree, threads: &[(&TtProject, &TtThread)]) -> String {
    indented_table(
        &["SPACE", "PROJECT", "THREAD", "ID"],
        threads
            .iter()
            .map(|(project, thread)| {
                vec![
                    space_name(tree, project).to_string(),
                    project.name.clone(),
                    thread.name.clone(),
                    thread.id.clone(),
                ]
            })
            .collect(),
    )
}

fn projects_table(tree: &ThinkTermTree, projects: &[&TtProject]) -> String {
    indented_table(
        &["SPACE", "PROJECT", "ID"],
        projects
            .iter()
            .map(|project| {
                vec![
                    space_name(tree, project).to_string(),
                    project.name.clone(),
                    project.id.clone(),
                ]
            })
            .collect(),
    )
}

/// Where the command's own pane ($WEZTERM_PANE) is, as far as the threads
/// go.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Caller {
    /// Not run in a ThinkTerm pane.
    Outside,
    /// In a pane of another server or window than the ones the threads are
    /// kept and shown by.
    Elsewhere,
    /// In a pane in this workspace.
    In(String),
}

/// The live project of the thread the caller's pane is in.
pub(crate) fn current_project<'a>(
    tree: &'a ThinkTermTree,
    caller: &Caller,
) -> Option<&'a TtProject> {
    match caller {
        Caller::In(workspace) => workspace_owner(tree, workspace)
            .map(|(project, _)| project)
            .filter(|project| project.archived_at.is_none()),
        Caller::Outside | Caller::Elsewhere => None,
    }
}

/// The Space of the caller's own thread, which a project's name is looked
/// for in first.
pub(crate) fn caller_space<'a>(tree: &'a ThinkTermTree, caller: &Caller) -> Option<&'a str> {
    current_project(tree, caller).map(|project| project.space_id.as_str())
}

pub(crate) fn no_project_here(server: &ThreadServer, tree: &ThinkTermTree) -> anyhow::Error {
    let why = match &server.caller {
        Caller::Outside => {
            "the command runs in no ThinkTerm pane, so there is no current thread to take one from"
        }
        Caller::Elsewhere => "the current pane is not one of the server's that keeps the threads",
        Caller::In(workspace) => match workspace_owner(tree, workspace) {
            Some(_) => "the current pane's thread is in an archived project",
            None => "the current pane is in no thread",
        },
    };
    UsageError(format!(
        "name the project with --project: {why} (`thinkterm cli thread list` shows the projects)"
    ))
    .into()
}

/// Make a thread called `name` in `project` on `server`, with its workspace
/// named as the server names one, and return it as the server took it -- a
/// name the project has already comes back numbered -- with the tree the
/// server answered with.
pub(crate) async fn create_thread(
    server: &Client,
    project: &TtProject,
    name: &str,
) -> anyhow::Result<(TtThread, ThinkTermTree)> {
    let name = name.trim();
    if name.is_empty() {
        return Err(UsageError("a thread needs a name".to_string()).into());
    }
    let thread_id = new_id("thread");
    let op = TreeOp::CreateThread {
        thread_id: thread_id.clone(),
        project_id: project.id.clone(),
        name: name.to_string(),
        workspace: Some(workspace_name(&project.id, &thread_id)),
        created_at: chrono::Utc::now().timestamp(),
    };
    let state = server
        .mutate_thinkterm_tree(MutateThinkTermTree { ops: vec![op] })
        .await
        .context("making the thread")?;
    let thread = state.tree.thread(&thread_id).cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "the server did not make the thread: the project {} may have been removed meanwhile",
            quoted(&project.name)
        )
    })?;
    Ok((thread, state.tree))
}

/// Remove `thread`, which this command made, unless something has opened
/// it meanwhile: a window in its workspace -- from the sidebar, or another
/// command -- is someone's, and the thread stays for it. Whether it was
/// removed.
pub(crate) async fn remove_unused_thread(
    server: &Client,
    thread: &TtThread,
) -> anyhow::Result<bool> {
    let workspace = thread_workspace(thread);
    let opened = server
        .list_panes()
        .await?
        .tabs
        .iter()
        .flat_map(|tab| tab.entries())
        .any(|entry| entry.workspace == workspace);
    if opened {
        return Ok(false);
    }
    server
        .mutate_thinkterm_tree(MutateThinkTermTree {
            ops: vec![TreeOp::DeleteThread {
                thread_id: thread.id.clone(),
            }],
        })
        .await
        .context("removing the thread")?;
    Ok(true)
}

/// The thread made at the same moment for the same name, when `made` came
/// back numbered: another one of the name `wanted` in its project that was
/// not there before (`before`), which two commands made together. The server
/// numbers the later, so the one named `wanted` came first and is the one
/// both keep.
pub(crate) fn made_meanwhile<'a>(
    after: &'a ThinkTermTree,
    before: &ThinkTermTree,
    made: &TtThread,
    wanted: &str,
) -> Option<&'a TtThread> {
    let wanted = wanted.trim();
    if made.name == wanted {
        return None;
    }
    after
        .project(&made.project_id)?
        .threads
        .iter()
        .find(|thread| {
            thread.id != made.id && thread.name == wanted && before.thread(&thread.id).is_none()
        })
}

/// Held while `spawn --thread` looks for its thread, makes it and puts its
/// program in it. Commands run at the same moment on this machine then find
/// the thread the first one made, and its window, rather than each making
/// its own window, or a copy of the thread (`made_meanwhile` is for a
/// command elsewhere). Let go when dropped, or when the process ends; one
/// that waits too long goes ahead without it.
pub(crate) struct ThreadLock {
    _file: Option<std::fs::File>,
}

pub(crate) async fn lock_threads() -> ThreadLock {
    let path = config::RUNTIME_DIR.join(config::runtime_file_name("thread-spawn.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path);
    let Ok(file) = file else {
        return ThreadLock { _file: None };
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match file.try_lock() {
            Ok(()) => return ThreadLock { _file: Some(file) },
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                smol::Timer::after(std::time::Duration::from_millis(20)).await;
            }
            Err(_) => return ThreadLock { _file: None },
        }
    }
}

/// An id shaped as the desktop and the browser mint theirs: the kind, the
/// time in milliseconds and eight hex digits, with no dashes in the random
/// part. The server mints its own as `kind-` and a dashed uuid, which the
/// desktop reads as rows of a landing it may drop.
fn new_id(kind: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    hasher.write_u32(std::process::id());
    format!(
        "{kind}-{}-{:08x}",
        chrono::Utc::now().timestamp_millis(),
        hasher.finish() as u32
    )
}

pub(crate) fn quoted(text: &str) -> String {
    format!("\"{}\"", printable(text))
}

/// `path` with its links followed, as far as it exists.
fn canonical(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

/// The process id of the GUI whose socket `socket` is (`gui-sock-PID`).
pub(crate) fn gui_pid_of_socket(socket: &Path) -> Option<u32> {
    socket
        .file_name()?
        .to_str()?
        .strip_prefix("gui-sock-")?
        .parse()
        .ok()
}

/// The ThinkTerm window `client` reached, when it reached one rather than a
/// mux server: its process id. The socket may have been named by a link to
/// it, such as the one the window publishes under its class.
pub(crate) fn reached_window(client: &Client) -> Option<u32> {
    gui_pid_of_socket(&canonical(client.unix_socket_path()?))
}

/// The server that keeps the threads the sidebar shows, a client of it, and
/// where the command's own pane is.
pub(crate) struct ThreadServer {
    /// The mux server whose tree holds the threads: they are read and made
    /// there.
    pub server: Client,
    /// The ThinkTerm window the command reached, when it reached one: panes
    /// are listed and spawned through it, so that a pane id printed is the
    /// window's, as the next command -- which reaches the window again --
    /// takes it.
    pub window: Option<Client>,
    pub caller: Caller,
}

impl ThreadServer {
    /// Where panes are listed and spawned.
    pub(crate) fn panes(&self) -> &Client {
        self.window.as_ref().unwrap_or(&self.server)
    }
}

/// The server that keeps the threads the sidebar shows.
///
/// A mux server keeps its own tree: `client` itself, when it reached one --
/// on a server, or in a pane of this machine's session server. A ThinkTerm
/// window keeps the threads of this machine's terminals in its own store,
/// and its own mux serves none of them; when it runs those terminals in a
/// background session server, that server holds the same threads and the
/// window takes in what changes there. So reaching a window, the threads
/// are that server's (`session_server_of_window`), and the panes still the
/// window's.
pub(crate) async fn thread_server(client: Client) -> anyhow::Result<ThreadServer> {
    let reached = server_id(&client).await?;
    match reached_window(&client) {
        None => {
            let caller = caller(&[(&client, reached.as_str(), false)]).await?;
            Ok(ThreadServer {
                server: client,
                window: None,
                caller,
            })
        }
        Some(window) => {
            let server = session_server_of_window(window).await?;
            let id = server_id(&server).await?;
            // A terminal the window runs itself, or one of the server's.
            let caller = caller(&[
                (&client, reached.as_str(), true),
                (&server, id.as_str(), false),
            ])
            .await?;
            Ok(ThreadServer {
                server,
                window: Some(client),
                caller,
            })
        }
    }
}

/// The id the mux `client` reached runs under, which the panes it spawns
/// carry as THINKTERM_MUX_SERVER_ID.
async fn server_id(client: &Client) -> anyhow::Result<String> {
    Ok(client
        .get_codec_version(codec::GetCodecVersion {})
        .await?
        .server_id)
}

/// Where the command's own pane is, among `ends`: each a client, the id
/// of the mux it reached, and whether that is a window.
async fn caller(ends: &[(&Client, &str, bool)]) -> anyhow::Result<Caller> {
    let Some(pane_id) = std::env::var("WEZTERM_PANE")
        .ok()
        .and_then(|pane| pane.trim().parse::<mux::pane::PaneId>().ok())
    else {
        return Ok(Caller::Outside);
    };
    let env_id = std::env::var("THINKTERM_MUX_SERVER_ID")
        .ok()
        .filter(|id| !id.is_empty());
    let env_socket = std::env::var_os("WEZTERM_UNIX_SOCKET")
        .filter(|socket| !socket.is_empty())
        .map(|socket| canonical(PathBuf::from(socket)));
    let sockets: Vec<Option<PathBuf>> = ends
        .iter()
        .map(|(client, _, _)| client.unix_socket_path().map(canonical))
        .collect();
    let described: Vec<(&str, Option<&Path>, bool)> = ends
        .iter()
        .zip(&sockets)
        .map(|((_, id, window), socket)| (*id, socket.as_deref(), *window))
        .collect();
    let Some(at) = pane_end(env_id.as_deref(), env_socket.as_deref(), &described) else {
        return Ok(Caller::Elsewhere);
    };
    let listing = ends[at].0.list_panes().await?;
    let workspace = listing
        .tabs
        .iter()
        .flat_map(|tab| tab.entries())
        .find(|entry| entry.pane_id == pane_id)
        .map(|entry| entry.workspace.clone());
    Ok(workspace.map_or(Caller::Elsewhere, Caller::In))
}

/// Which of `ends` -- each the id of a mux, its socket, and whether it is a
/// window -- the command's own pane is in: the one whose id the pane's
/// environment names (THINKTERM_MUX_SERVER_ID, which every pane is spawned
/// with, and which no socket chosen later changes); else the one at the
/// socket it names (WEZTERM_UNIX_SOCKET); else, for a pane named with
/// neither -- a plugin's command names its terminal on this machine's
/// session server so, and finds the server with --prefer-mux -- the server,
/// when it is the only end. Beside a window it could be either: the two
/// number their panes each for itself, so the same id is no sign of which.
fn pane_end(
    env_id: Option<&str>,
    env_socket: Option<&Path>,
    ends: &[(&str, Option<&Path>, bool)],
) -> Option<usize> {
    if let Some(id) = env_id {
        return ends.iter().position(|(end, _, _)| *end == id);
    }
    if let Some(socket) = env_socket {
        return ends.iter().position(|(_, at, _)| *at == Some(socket));
    }
    match ends {
        [(_, _, false)] => Some(0),
        _ => None,
    }
}

/// The background session server the window `window` (its process id) runs
/// this machine's terminals in, and keeps their threads in: one it is a
/// client of, when the window runs its terminals there at all. Never started
/// for this: a server the window does not use is no place for threads it
/// should show.
pub(crate) async fn session_server_of_window(window: u32) -> anyhow::Result<Client> {
    let config = config::configuration();
    // As the window decides it (`local_sessions::install`): a configured
    // default domain keeps the terminals there, and the setting decides the
    // rest. Being a client of the server is not enough: a window can attach
    // to it as to any server and still run its own terminals itself.
    if let Some(domain) = &config.default_domain {
        return Err(anyhow::anyhow!(
            "the ThinkTerm window on this machine starts its terminals in the domain {} \
             (default_domain in the configuration), not in a background session server, \
             so threads cannot be listed or made from the command line",
            quoted(domain)
        ));
    }
    // The setting is taken in when the window starts. Turned off since, it
    // still has its terminals in the session server until it restarts; the
    // mark it left when it attached the server says so.
    if !local_sessions_setting(&config::native_settings_path()) && !session_host_marked() {
        return Err(window_keeps_its_threads(
            "the setting for one is off".to_string(),
        ));
    }
    let Some(domain) = session_server_domain(&config.unix_domains) else {
        return Err(window_keeps_its_threads(
            "no unix domain is configured for one".to_string(),
        ));
    };
    let socket = domain.socket_path();
    let mut ui = mux::connui::ConnectionUI::new_headless();
    let server = match Client::new_unix_domain(None, &domain, true, &mut ui, true) {
        Ok(server) => server,
        Err(err) => {
            return Err(window_keeps_its_threads(format!(
                "none answers at {}: {err:#}",
                socket.display()
            )))
        }
    };
    server
        .verify_version_compat(&ui)
        .await
        .context(ServerUnreachable)?;
    let here = hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .unwrap_or_default();
    let clients = server.list_clients().await?.clients;
    let its = clients.iter().any(|info| {
        let host = info.client_id.hostname.as_str();
        info.client_id.pid == window
            && (host == here || host.starts_with(&format!("{here} (via proxy pid ")))
    });
    if its {
        Ok(server)
    } else {
        Err(window_keeps_its_threads(format!(
            "the one at {} is not the window's",
            socket.display()
        )))
    }
}

/// Settings > General > Session server, as a window that started with
/// `settings` (settings.json) has it: on when there is no such file yet,
/// as for a new install, and otherwise only when the file turns it on --
/// as `native_settings` reads it.
fn local_sessions_setting(settings: &Path) -> bool {
    let wanted = |text: &[u8]| {
        serde_json::from_slice::<serde_json::Value>(text)
            .ok()
            .and_then(|settings| {
                settings
                    .pointer("/workspaces/local_sessions_via_mux")
                    .and_then(serde_json::Value::as_bool)
            })
            .unwrap_or(false)
    };
    match std::fs::read(settings) {
        Ok(text) => wanted(&text),
        Err(err) => err.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Whether a window has attached this machine's session server as the one
/// it runs its terminals in, and nothing has stopped it since: the mark the
/// window leaves (`local_sessions::note_host_attached`), which only a server
/// the setting runs gets -- not one a window merely connects to.
fn session_host_marked() -> bool {
    config::RUNTIME_DIR
        .join(config::runtime_file_name("local-session-host"))
        .exists()
}

/// The tree of the session server the window `window` keeps its threads in,
/// as long as it answers within `within_window_wait`: a check before `spawn
/// --workspace` is not held up by a server that does not answer.
pub(crate) async fn session_tree_of_window(window: u32) -> Option<ThinkTermTree> {
    crate::cli::within_window_wait(async {
        let server = session_server_of_window(window).await.ok()?;
        server
            .get_thinkterm_tree()
            .await
            .ok()
            .map(|state| state.tree)
    })
    .await
}

/// The unix domain a window runs this machine's terminals in, when it does:
/// the one named `unix`, else the first that reaches a socket rather than
/// going through a proxy (`local_sessions::candidate` in the GUI).
pub(crate) fn session_server_domain(domains: &[config::UnixDomain]) -> Option<config::UnixDomain> {
    domains
        .iter()
        .find(|domain| domain.name == "unix" && domain.proxy_command.is_none())
        .or_else(|| domains.iter().find(|domain| domain.proxy_command.is_none()))
        .cloned()
}

fn window_keeps_its_threads(detail: String) -> anyhow::Error {
    anyhow::anyhow!(
        "the ThinkTerm window on this machine runs its terminals itself and keeps their \
         threads with them, so threads cannot be listed or made from the command line \
         (it uses no background session server: {detail}). Turn on Settings > General > \
         Session server > \"Keep local terminals in a background session server\"; it \
         takes effect the next time ThinkTerm starts"
    )
}

/// Whether `client` reached its server's socket on this machine, rather
/// than through a proxy command: then its server sees this machine's files.
pub(crate) fn on_this_machine(client: &Client) -> bool {
    client.unix_socket_path().is_some()
}

#[cfg(test)]
mod test {
    use super::*;

    fn tree() -> ThinkTermTree {
        let mut tree = ThinkTermTree::default();
        for op in [
            TreeOp::CreateSpace {
                space_id: "space-1".into(),
                name: "Work".into(),
            },
            TreeOp::CreateSpace {
                space_id: "space-2".into(),
                name: "Play".into(),
            },
            TreeOp::CreateProject {
                project_id: "project-1".into(),
                space_id: "space-1".into(),
                name: "thinkterm".into(),
                path: "/src/thinkterm".into(),
            },
            TreeOp::CreateProject {
                project_id: "project-2".into(),
                space_id: "space-1".into(),
                name: "Home".into(),
                path: "~".into(),
            },
            TreeOp::CreateProject {
                project_id: "project-3".into(),
                space_id: "space-1".into(),
                name: "old".into(),
                path: "/old".into(),
            },
            TreeOp::CreateProject {
                project_id: "project-4".into(),
                space_id: "space-2".into(),
                name: "Home".into(),
                path: "~".into(),
            },
            TreeOp::CreateThread {
                thread_id: "thread-1".into(),
                project_id: "project-1".into(),
                name: "agent".into(),
                workspace: Some("thinkterm:project-1:thread-1".into()),
                created_at: 1,
            },
            TreeOp::CreateThread {
                thread_id: "thread-2".into(),
                project_id: "project-2".into(),
                name: "agent".into(),
                workspace: None,
                created_at: 2,
            },
            TreeOp::CreateThread {
                thread_id: "thread-3".into(),
                project_id: "project-3".into(),
                name: "kept".into(),
                workspace: Some("custom".into()),
                created_at: 3,
            },
            TreeOp::CreateThread {
                thread_id: "thread-4".into(),
                project_id: "project-4".into(),
                name: "solo".into(),
                workspace: None,
                created_at: 4,
            },
            TreeOp::SetProjectArchived {
                project_id: "project-3".into(),
                archived_at: Some(9),
            },
        ] {
            assert!(codec::apply_op(&mut tree, &op), "{:?}", op);
        }
        tree
    }

    #[test]
    fn a_workspace_is_the_thread_s_that_opens_it() {
        let tree = tree();
        let by_record = workspace_owner(&tree, "thinkterm:project-1:thread-1");
        assert_eq!(
            by_record.map(|(_, thread)| thread.id.as_str()),
            Some("thread-1")
        );
        // Created with no workspace: the server's name for it is the one it
        // opens.
        let by_name = workspace_owner(&tree, "thinkterm:project-2:thread-2");
        assert_eq!(
            by_name.map(|(_, thread)| thread.id.as_str()),
            Some("thread-2")
        );
        assert_eq!(
            thread_workspace(tree.thread("thread-2").unwrap()),
            "thinkterm:project-2:thread-2"
        );
        // A thread that opens a workspace of its own is not the one the
        // server's name for it would be: selecting it opens the other.
        assert!(workspace_owner(&tree, "thinkterm:project-3:thread-3").is_none());
        assert!(workspace_owner(&tree, "thinkterm:project-1:thread-9").is_none());
        assert!(workspace_owner(&tree, "default").is_none());
        // An archived project's thread still owns its workspace; the check
        // in `spawn` refuses it for that reason.
        let archived = workspace_owner(&tree, "custom").map(|(project, _)| project.archived_at);
        assert_eq!(archived, Some(Some(9)));
        assert!(names_a_thread("thinkterm:project-1:thread-9"));
        assert!(!names_a_thread("default"));
    }

    #[test]
    fn projects_are_found_by_id_or_by_a_name_only_one_live_project_has() {
        let tree = tree();
        assert_eq!(find_project(&tree, "project-2", None).unwrap().name, "Home");
        assert_eq!(
            find_project(&tree, "thinkterm", None).unwrap().id,
            "project-1"
        );
        let missing = find_project(&tree, "nope", None).unwrap_err();
        assert!(
            missing.downcast_ref::<NoSuchTarget>().is_some(),
            "{:#}",
            missing
        );
        // An archived project is not one to put work in.
        let archived = find_project(&tree, "project-3", None).unwrap_err();
        assert!(archived.to_string().contains("archived"), "{:#}", archived);
        assert!(find_project(&tree, "old", None).is_err());
    }

    #[test]
    fn a_name_two_spaces_share_is_the_caller_s_space_s_or_listed() {
        let tree = tree();
        // "Home" is in both Spaces.
        let listed = find_project(&tree, "Home", None).unwrap_err();
        assert!(
            listed.downcast_ref::<NoSuchTarget>().is_some(),
            "{:#}",
            listed
        );
        let text = listed.to_string();
        for part in ["Work", "Play", "project-2", "project-4"] {
            assert!(text.contains(part), "{} in {}", part, text);
        }
        assert_eq!(
            find_project(&tree, "Home", Some("space-2")).unwrap().id,
            "project-4"
        );
        assert_eq!(
            find_project(&tree, " Home ", Some("space-1")).unwrap().id,
            "project-2"
        );
    }

    #[test]
    fn a_thread_s_name_is_looked_for_in_the_caller_s_project_first() {
        let tree = tree();
        let thinkterm = tree.project("project-1").unwrap();
        let home = tree.project("project-2").unwrap();
        // Its own project's "agent", though another project has one too.
        let mine = find_thread(&tree, "agent", Scope::Caller(home)).unwrap();
        assert_eq!(mine.map(|thread| thread.id.as_str()), Some("thread-2"));
        let mine = find_thread(&tree, " agent ", Scope::Caller(thinkterm)).unwrap();
        assert_eq!(mine.map(|thread| thread.id.as_str()), Some("thread-1"));
        // A name only another project's thread has is listed, not taken,
        // and no thread is made for it.
        let elsewhere = find_thread(&tree, "solo", Scope::Caller(home)).unwrap_err();
        assert!(
            elsewhere.downcast_ref::<NoSuchTarget>().is_some(),
            "{:#}",
            elsewhere
        );
        let text = elsewhere.to_string();
        for part in ["Play", "Home", "solo", "thread-4", "--project project-2"] {
            assert!(text.contains(part), "{} in {}", part, text);
        }
        // A name no thread has: one is made in the caller's project.
        assert!(find_thread(&tree, "new one", Scope::Caller(home))
            .unwrap()
            .is_none());
        // With no project to start from, a name only one thread has is it.
        let only = find_thread(&tree, "solo", Scope::Anywhere).unwrap();
        assert_eq!(only.map(|thread| thread.id.as_str()), Some("thread-4"));
    }

    #[test]
    fn a_name_several_threads_have_is_listed_with_their_spaces_and_ids() {
        let tree = tree();
        let shared = find_thread(&tree, "agent", Scope::Anywhere).unwrap_err();
        assert!(
            shared.downcast_ref::<NoSuchTarget>().is_some(),
            "{:#}",
            shared
        );
        let text = shared.to_string();
        for part in ["SPACE", "Work", "thinkterm", "Home", "thread-1", "thread-2"] {
            assert!(text.contains(part), "{} in {}", part, text);
        }
        // Threads of an archived project are not found by name, nor used by id.
        assert!(find_thread(&tree, "kept", Scope::Anywhere)
            .unwrap()
            .is_none());
        assert!(find_thread(&tree, "thread-3", Scope::Anywhere).is_err());
    }

    #[test]
    fn an_id_is_its_thread_anywhere_but_must_be_in_the_project_given() {
        let tree = tree();
        let home = tree.project("project-2").unwrap();
        // From the caller's project, an id still names a thread elsewhere.
        let other = find_thread(&tree, "thread-1", Scope::Caller(home)).unwrap();
        assert_eq!(other.map(|thread| thread.name.as_str()), Some("agent"));
        let given = find_thread(&tree, "thread-1", Scope::Given(home)).unwrap_err();
        assert!(given.downcast_ref::<UsageError>().is_some(), "{:#}", given);
        // A name given a project is looked for there alone.
        assert!(find_thread(&tree, "solo", Scope::Given(home))
            .unwrap()
            .is_none());
    }

    #[test]
    fn what_is_listed_cannot_drive_the_terminal() {
        let mut tree = tree();
        assert!(codec::apply_op(
            &mut tree,
            &TreeOp::CreateThread {
                thread_id: "thread-\u{1b}[2J".into(),
                project_id: "project-4".into(),
                name: "agent".into(),
                workspace: None,
                created_at: 5,
            }
        ));
        let shared = find_thread(&tree, "agent", Scope::Anywhere).unwrap_err();
        let text = shared.to_string();
        assert!(!text.contains('\u{1b}'), "{:?}", text);
        assert!(text.contains("thread-\\u{1b}[2J"), "{:?}", text);
    }

    #[test]
    fn new_ids_are_shaped_as_the_desktop_mints_them() {
        let id = new_id("thread");
        let mut parts = id.splitn(3, '-');
        assert_eq!(parts.next(), Some("thread"));
        assert!(parts.next().unwrap().bytes().all(|b| b.is_ascii_digit()));
        let random = parts.next().unwrap();
        assert_eq!(random.len(), 8);
        assert!(random.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(new_id("thread"), new_id("thread"));
    }

    #[test]
    fn a_window_socket_names_its_process_through_a_link_too() {
        assert_eq!(
            gui_pid_of_socket(Path::new("/run/user/1/thinkterm/gui-sock-4242")),
            Some(4242)
        );
        assert_eq!(
            gui_pid_of_socket(Path::new("/run/user/1/thinkterm/sock")),
            None
        );
        assert_eq!(gui_pid_of_socket(Path::new("gui-sock-x")), None);
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("gui-sock-4242");
        std::fs::write(&socket, b"").unwrap();
        let link = dir.path().join("x11-:0-org.example");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&socket, &link).unwrap();
            assert_eq!(gui_pid_of_socket(&canonical(link)), Some(4242));
        }
    }

    #[test]
    fn the_caller_s_pane_is_where_its_server_id_says() {
        let window = Path::new("/run/thinkterm/gui-sock-1");
        let server = Path::new("/run/thinkterm/sock");
        let ends = [("gui", Some(window), true), ("mux", Some(server), false)];
        // The id wins over a socket chosen since.
        assert_eq!(pane_end(Some("mux"), Some(window), &ends), Some(1));
        assert_eq!(pane_end(Some("gui"), None, &ends), Some(0));
        assert_eq!(pane_end(Some("another"), Some(server), &ends), None);
        // Without an id, the socket.
        assert_eq!(pane_end(None, Some(window), &ends), Some(0));
        assert_eq!(pane_end(None, Some(Path::new("/elsewhere")), &ends), None);
        // With neither, as a plugin names its terminal: the server's, when
        // no window numbers its panes beside it.
        assert_eq!(pane_end(None, None, &ends[1..]), Some(0));
        assert_eq!(pane_end(None, None, &ends), None);
        assert_eq!(pane_end(None, None, &ends[..1]), None);
    }

    #[test]
    fn the_session_server_setting_is_read_as_the_window_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        // No file yet: a new install, which has it on.
        assert!(local_sessions_setting(&settings));
        for (text, on) in [
            (r#"{"workspaces":{"local_sessions_via_mux":true}}"#, true),
            (r#"{"workspaces":{"local_sessions_via_mux":false}}"#, false),
            (r#"{"workspaces":{}}"#, false),
            (r#"{"version":1}"#, false),
            ("not json", false),
        ] {
            std::fs::write(&settings, text).unwrap();
            assert_eq!(local_sessions_setting(&settings), on, "{}", text);
        }
    }

    #[test]
    fn the_session_server_is_the_unix_domain_or_the_first_without_a_proxy() {
        let domain = |name: &str, proxy: bool| config::UnixDomain {
            name: name.into(),
            proxy_command: proxy.then(|| vec!["nc".into()]),
            ..Default::default()
        };
        let picked = session_server_domain(&[
            domain("other", true),
            domain("mine", false),
            domain("unix", false),
        ]);
        assert_eq!(picked.unwrap().name, "unix");
        let picked = session_server_domain(&[domain("unix", true), domain("mine", false)]);
        assert_eq!(picked.unwrap().name, "mine");
        assert!(session_server_domain(&[domain("unix", true)]).is_none());
    }

    #[test]
    fn a_thread_made_twice_at_once_is_the_first_one() {
        let before = tree();
        let mut after = before.clone();
        for op in [
            TreeOp::CreateThread {
                thread_id: "thread-a".into(),
                project_id: "project-2".into(),
                name: "review".into(),
                workspace: None,
                created_at: 4,
            },
            TreeOp::CreateThread {
                thread_id: "thread-b".into(),
                project_id: "project-2".into(),
                name: "review".into(),
                workspace: None,
                created_at: 4,
            },
        ] {
            assert!(codec::apply_op(&mut after, &op));
        }
        let first = after.thread("thread-a").unwrap();
        let second = after.thread("thread-b").unwrap();
        assert_eq!(second.name, "review 2");
        assert!(made_meanwhile(&after, &before, first, "review").is_none());
        let kept = made_meanwhile(&after, &before, second, "review").unwrap();
        assert_eq!(kept.id, "thread-a");
        // A numbered name that was asked for as is, or one already there
        // before, is no race.
        assert!(made_meanwhile(&after, &after, second, "review").is_none());
    }

    #[test]
    fn a_thread_s_program_starts_in_its_project_s_directory() {
        let tree = tree();
        assert_eq!(
            project_dir(tree.project("project-1").unwrap()).as_deref(),
            Some("/src/thinkterm")
        );
        // The server expands `~` against its own home.
        assert_eq!(
            project_dir(tree.project("project-2").unwrap()).as_deref(),
            Some("~")
        );
        let mut server_row = tree.project("project-2").unwrap().clone();
        server_row.path = "wezterm-mux://local".into();
        assert_eq!(project_dir(&server_row), None);
        server_row.path = "  ".into();
        assert_eq!(project_dir(&server_row), None);
    }
}
