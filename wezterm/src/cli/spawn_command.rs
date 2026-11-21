use crate::cli::thread::{
    create_thread, current_project, find_project, find_thread, lock_threads, made_meanwhile,
    names_a_thread, no_project_here, on_this_machine, project_dir, quoted, reached_window,
    remove_unused_thread, session_server_domain, session_tree_of_window, thread_server,
    thread_workspace, workspace_owner, Scope, ThreadServer,
};
use crate::cli::{printable, resolve_relative_cwd, NoSuchTarget, UsageError};
use anyhow::Context;
use clap::{Parser, ValueHint};
use config::keyassignment::SpawnTabDomain;
use config::ConfigHandle;
use mux::pane::PaneId;
use mux::command_spec::{CommandSpec, CommandSpecExt};
use mux::domain::RequiredCwdUnavailable;
use mux::window::WindowId;
use portable_pty::cmdbuilder::CommandBuilder;
use std::ffi::OsString;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct SpawnCommand {
    /// Specify the current pane.
    /// The default is to use the current pane based on the
    /// environment variable WEZTERM_PANE.
    /// The pane is used to determine the current domain
    /// and window.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// The pane by name rather than id: its label, its thread's name (a
    /// thread with one pane), or `thread/label`.
    #[arg(long, conflicts_with = "pane_id")]
    pane: Option<String>,

    #[arg(long)]
    domain_name: Option<String>,

    /// Specify the window into which to spawn a tab.
    /// If omitted, the window associated with the current
    /// pane is used.
    /// Cannot be used with `--workspace` or `--new-window`.
    #[arg(long, conflicts_with_all=&["workspace", "new_window"])]
    window_id: Option<WindowId>,

    /// Spawn into a new window, rather than a new tab.
    #[arg(long)]
    new_window: bool,

    /// Specify the current working directory for the initially
    /// spawned program
    #[arg(long, value_parser, value_hint=ValueHint::DirPath)]
    cwd: Option<OsString>,

    /// When creating a new window, override the default workspace name
    /// with the provided name.  The default name is "default".
    /// Requires `--new-window`. This names the mux workspace only and makes
    /// no thread: ThinkTerm's sidebar shows a window only in a workspace a
    /// thread has, so a `thinkterm:...` name no thread has is refused before
    /// anything is spawned, and another name no thread has is spawned with a
    /// warning. To put the program in a thread, use `--thread`.
    #[arg(long, requires = "new_window")]
    workspace: Option<String>,

    /// Put the program in this thread: its id, which names it wherever it
    /// is, or its name, looked for in `--project`, else in the project of
    /// the current pane's thread, else in every project. When several
    /// threads have the name, or only other projects than the current
    /// pane's do, they are listed and nothing is started. A name no thread
    /// has makes a thread of it in that project. A thread with no window
    /// open, unless it was made just now, is opened first as the sidebar
    /// opens it (its saved tabs, or a shell). The program goes in a new tab
    /// of the thread's window, or in a window of its own with
    /// `--new-window`, and starts in the project's directory, or the one
    /// `--cwd` names: nothing is started when that cannot be opened.
    #[arg(long, conflicts_with_all = &["workspace", "window_id", "pane_id", "pane"])]
    thread: Option<String>,

    /// With `--thread`: the project the thread is in, or is to be made in:
    /// its id, or its name. A name several projects have is the one in the
    /// current pane's Space, when just one is there; otherwise they are
    /// listed.
    #[arg(long, requires = "thread")]
    project: Option<String>,

    /// Wait for the program to end, and exit with its status. The pane
    /// id is printed first, as without it.
    #[arg(long)]
    wait: bool,

    /// With --wait, give up after this long, exiting with status 124: a
    /// number of seconds, or a number with s, m or h after it. `--for` is
    /// the same.
    #[arg(
        long,
        visible_alias = "for",
        value_parser = crate::cli::wait::parse_timeout,
        requires = "wait"
    )]
    timeout: Option<std::time::Duration>,

    /// Label the new pane, so that `--pane LABEL` finds it.
    #[arg(long)]
    label: Option<String>,

    /// Instead of executing your shell, run PROG.
    /// For example: `thinkterm cli spawn -- bash -l` will spawn bash
    /// as if it were a login shell.
    #[arg(value_parser, value_hint=ValueHint::CommandWithArguments, num_args=1..)]
    prog: Vec<OsString>,
}

impl SpawnCommand {
    /// With `--thread`: the program goes in a thread of the server's.
    pub fn puts_in_thread(&self) -> bool {
        self.thread.is_some()
    }

    pub async fn run(self, client: Client, config: &ConfigHandle) -> anyhow::Result<()> {
        // Checked before anything is spawned.
        let label = match &self.label {
            Some(text) => crate::cli::set_pane_label::checked_label(text)?,
            None => None,
        };
        if self.thread.is_some() {
            return self.run_in_thread(client, config, label).await;
        }
        if let Some(workspace) = &self.workspace {
            check_workspace(&client, workspace, self.domain_name.as_deref()).await?;
        }
        let window_id = if self.new_window {
            None
        } else {
            match self.window_id {
                Some(w) => Some(w),
                None => {
                    let pane_id =
                        crate::cli::target::resolve(&client, self.pane_id, self.pane.as_deref())
                            .await?;

                    let panes = client.list_panes().await?;
                    let mut window_id = None;
                    'outer: for tabroot in panes.tabs {
                        let mut cursor = tabroot.into_tree().cursor();

                        loop {
                            if let Some(stack) = cursor.leaf_mut() {
                                for entry in &stack.panes {
                                    if entry.pane_id == pane_id {
                                        window_id.replace(entry.window_id);
                                        break 'outer;
                                    }
                                }
                            }
                            match cursor.preorder_next() {
                                Ok(c) => cursor = c,
                                Err(_) => break,
                            }
                        }
                    }
                    window_id
                }
            }
        };

        let workspace = self
            .workspace
            .as_deref()
            .unwrap_or(
                config
                    .default_workspace
                    .as_deref()
                    .unwrap_or(mux::DEFAULT_WORKSPACE),
            )
            .to_string();

        let size = config.initial_size(0, None);

        let spawned = client
            .spawn_v2(codec::SpawnV2 {
                domain: self
                    .domain_name
                    .map_or(SpawnTabDomain::DefaultDomain, |name| {
                        SpawnTabDomain::DomainName(name)
                    }),
                window_id,
                command: if self.prog.is_empty() {
                    None
                } else {
                    let builder = CommandBuilder::from_argv(self.prog);
                    Some(CommandSpec::from_command_builder(&builder))
                },
                command_dir: resolve_relative_cwd(self.cwd)?,
                size,
                workspace,
            })
            .await?;

        log::debug!("{:?}", spawned);
        finish(&client, spawned, label, self.wait, self.timeout).await
    }

    /// `--thread`: the program goes in the thread, which is made first when
    /// none has the name, on the server that keeps the threads the sidebar
    /// shows.
    async fn run_in_thread(
        self,
        client: Client,
        config: &ConfigHandle,
        label: Option<String>,
    ) -> anyhow::Result<()> {
        let spec = self
            .thread
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string();
        if spec.is_empty() {
            return Err(UsageError("--thread needs a thread's id or name".to_string()).into());
        }
        let server = thread_server(client).await?;
        // --cwd is settled where the command runs, before anything is made
        // or opened.
        let cwd = resolve_relative_cwd(self.cwd.clone())?;
        let lock = lock_threads().await;
        let before = server.server.get_thinkterm_tree().await?.tree;
        // A name is looked for in `--project`, else in the project of the
        // caller's own thread, else -- outside any -- in every project; an
        // id is the thread it names wherever it is (`find_thread`).
        let own = current_project(&before, &server.caller);
        let scope = match &self.project {
            Some(project) => Scope::Given(find_project(
                &before,
                project,
                own.map(|own| own.space_id.as_str()),
            )?),
            None => own.map_or(Scope::Anywhere, Scope::Caller),
        };
        let found = find_thread(&before, &spec, scope)?;
        let project = match found {
            Some(thread) => before.project_of_thread(&thread.id),
            None => Some(
                scope
                    .project()
                    .ok_or_else(|| no_project_here(&server, &before))?,
            ),
        };
        // The project's directory, or the one --cwd names, and no other: a
        // program started elsewhere than asked -- as it would be in the
        // home directory when that one is missing -- can do harm there. So
        // nothing is made or opened for a program that cannot start: seen
        // to here when the server sees this machine's files, and by the
        // server again as it spawns.
        let command_dir = cwd.or_else(|| project.and_then(project_dir));
        if let Some(dir) = &command_dir {
            if on_this_machine(&server.server) {
                directory_opens(dir)?;
            }
        }
        // `made`: by this command, which removes it again if nothing goes in
        // it. `new`: made just now, here or by another command, and so with
        // no saved tabs to open first.
        let (thread, made, new) = match found {
            Some(thread) => (thread.clone(), false, false),
            None => {
                let project = scope
                    .project()
                    .ok_or_else(|| no_project_here(&server, &before))?;
                let (made_here, after) = create_thread(&server.server, project, &spec).await?;
                match made_meanwhile(&after, &before, &made_here, &spec) {
                    // Another command made it at the same moment: that one
                    // is used, and the copy made here goes.
                    Some(first) => {
                        if let Err(err) = remove_unused_thread(&server.server, &made_here).await {
                            eprintln!(
                                "the thread {} ({}) made beside {} was left: {:#}",
                                quoted(&made_here.name),
                                printable(&made_here.id),
                                printable(&first.id),
                                err
                            );
                        }
                        (first.clone(), false, true)
                    }
                    None => {
                        eprintln!(
                            "made the thread {} ({}) in the project {}",
                            quoted(&made_here.name),
                            printable(&made_here.id),
                            quoted(&project.name)
                        );
                        (made_here, true, true)
                    }
                }
            }
        };
        let (wait, timeout) = (self.wait, self.timeout);
        let spawned = self
            .spawn_in_thread(&server, config, &thread, command_dir, new)
            .await;
        let spawned = match spawned {
            Ok(spawned) => spawned,
            Err(err) => {
                // Nothing went in it: a thread made only for this goes too,
                // unless something opened it meanwhile.
                if made {
                    match remove_unused_thread(&server.server, &thread).await {
                        Ok(true) => eprintln!(
                            "removed the thread {} ({}) again",
                            quoted(&thread.name),
                            printable(&thread.id)
                        ),
                        Ok(false) => eprintln!(
                            "the thread {} ({}) was kept: it was opened meanwhile",
                            quoted(&thread.name),
                            printable(&thread.id)
                        ),
                        Err(removal) => eprintln!(
                            "the thread {} ({}) was left empty: {:#}",
                            quoted(&thread.name),
                            printable(&thread.id),
                            removal
                        ),
                    }
                }
                return Err(err);
            }
        };
        drop(lock);
        log::debug!("{:?}", spawned);
        // Its id as the window has it, when the command reached one: the
        // next command reaches the window again.
        finish(server.panes(), spawned, label, wait, timeout).await
    }

    async fn spawn_in_thread(
        self,
        server: &ThreadServer,
        config: &ConfigHandle,
        thread: &codec::TtThread,
        command_dir: Option<String>,
        new: bool,
    ) -> anyhow::Result<codec::SpawnResponse> {
        let size = config.initial_size(0, None);
        let panes = server.panes();
        let mut workspace = thread_workspace(thread);
        let mut window_id = live_window(panes, &workspace).await?;
        if window_id.is_none() && !new {
            // A thread with no window open is opened as it was -- its saved
            // tabs, or one shell -- as the sidebar opens it, and the program
            // goes in beside them. A bare window put in its workspace instead
            // would be taken for its layout, and the saved one lost.
            //
            // One gone meanwhile -- removed on another device, say -- is not
            // asked for: the server would open another in its place.
            let now = server.server.get_thinkterm_tree().await?.tree;
            if now.thread(&thread.id).is_none() {
                return Err(NoSuchTarget(format!(
                    "the thread {} was removed meanwhile; nothing was started",
                    quoted(&thread.name)
                ))
                .into());
            }
            let opened = server
                .server
                .ensure_thinkterm_thread(codec::EnsureThinkTermThread {
                    preferred_thread_id: Some(thread.id.clone()),
                    size,
                })
                .await
                .context("opening the thread")?;
            if opened.thread_id != thread.id {
                anyhow::bail!(
                    "the thread {} was gone when the server came to open it, and the server \
                     opened its thread {} instead; the program was not started",
                    quoted(&thread.name),
                    printable(&opened.thread_id)
                );
            }
            workspace = opened.workspace;
            // A window takes in what its server opened a moment later. With
            // --new-window the program has a window of its own anyway.
            if !self.new_window {
                window_id = window_when_there(panes, &workspace, server.window.is_some()).await?;
                if window_id.is_none() && server.window.is_some() {
                    eprintln!(
                        "the ThinkTerm window had not taken in the thread's window yet; the \
                         program has a window of its own beside it"
                    );
                }
            }
        }
        let command = match (self.prog.is_empty(), command_dir.is_some()) {
            (true, false) => None,
            (default_prog, required) => {
                // The shell, as for `None`, when no program is given.
                let mut builder = if default_prog {
                    CommandBuilder::new_default_prog()
                } else {
                    CommandBuilder::from_argv(self.prog)
                };
                builder.set_require_cwd(required);
                Some(CommandSpec::from_command_builder(&builder))
            }
        };
        panes
            .spawn_v2(codec::SpawnV2 {
                domain: self
                    .domain_name
                    .map_or(SpawnTabDomain::DefaultDomain, |name| {
                        SpawnTabDomain::DomainName(name)
                    }),
                // A new tab of the thread's window, unless asked for a window
                // of its own.
                window_id: if self.new_window { None } else { window_id },
                command,
                command_dir,
                size,
                workspace,
            })
            .await
            .map_err(|err| {
                // Said plainly rather than in the server's words, which
                // give everything the spawn was asked with.
                match RequiredCwdUnavailable::from_message(&format!("{:#}", err)) {
                    Some(missing) => {
                        cannot_start_in(&missing.dir.to_string_lossy(), &missing.detail)
                    }
                    None => err,
                }
            })
    }
}

/// Whether the directory `dir` opens, as the server requires it to for the
/// program: with `~` the home directory, as the server expands it
/// (`build_command`) -- this machine's, which is the server's when it sees
/// this machine's files.
fn directory_opens(dir: &str) -> anyhow::Result<()> {
    let path = if dir == "~" || dir.starts_with("~/") {
        config::HOME_DIR.join(dir.strip_prefix("~/").unwrap_or(""))
    } else {
        std::path::PathBuf::from(dir)
    };
    match std::fs::read_dir(&path) {
        Ok(_) => Ok(()),
        Err(err) => Err(cannot_start_in(&path.to_string_lossy(), &err.to_string())),
    }
}

fn cannot_start_in(dir: &str, why: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "the program was not started: its directory {} cannot be opened ({}); fix the \
         project's folder, or name another with --cwd",
        quoted(dir),
        why
    )
}

/// The first window of `workspace` on `server`, when it has one.
async fn live_window(server: &Client, workspace: &str) -> anyhow::Result<Option<WindowId>> {
    Ok(server
        .list_panes()
        .await?
        .tabs
        .iter()
        .flat_map(|tab| tab.entries())
        .find(|entry| entry.workspace == workspace)
        .map(|entry| entry.window_id))
}

/// The first window of `workspace` among `panes`', waited for a while when
/// `wait`: a ThinkTerm window takes in a window its session server opened
/// once it hears of it.
async fn window_when_there(
    panes: &Client,
    workspace: &str,
    wait: bool,
) -> anyhow::Result<Option<WindowId>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let window = live_window(panes, workspace).await?;
        if window.is_some() || !wait || std::time::Instant::now() >= deadline {
            return Ok(window);
        }
        smol::Timer::after(std::time::Duration::from_millis(50)).await;
    }
}

/// Before anything is spawned in the workspace `workspace` in the domain
/// `domain`: whether a thread has it, so that ThinkTerm shows its window. A
/// mux server's tree says. A window keeps the threads of this machine's
/// terminals itself, and when it keeps them in a background session server,
/// that server's tree says; another domain of the window's is a server
/// whose threads the window names only while it has them open, as it names
/// its own when it keeps them itself.
async fn check_workspace(
    client: &Client,
    workspace: &str,
    domain: Option<&str>,
) -> anyhow::Result<()> {
    /// What says whether a thread has the workspace.
    enum Says {
        /// A tree of every thread it could be.
        Tree(codec::ThinkTermTree),
        /// A window's names for what it has open: whether it named it.
        Window(bool),
    }
    let says: anyhow::Result<Says> = async {
        let Some(window) = reached_window(client) else {
            return Ok(Says::Tree(client.get_thinkterm_tree().await?.tree));
        };
        let config = config::configuration();
        let this_machine = domain.map_or(true, |name| {
            name == "local"
                || session_server_domain(&config.unix_domains).is_some_and(|host| host.name == name)
        });
        if this_machine {
            if let Some(tree) = session_tree_of_window(window).await {
                return Ok(Says::Tree(tree));
            }
        }
        let names =
            crate::cli::within_window_wait(async { client.get_workspace_names().await.ok() })
                .await
                .ok_or_else(|| {
                    anyhow::anyhow!("the ThinkTerm window did not name its workspaces")
                })?;
        Ok(Says::Window(
            names.names.iter().any(|name| name.workspace == workspace),
        ))
    }
    .await;
    match says {
        Ok(Says::Tree(tree)) => check_in_tree(&tree, workspace),
        Ok(Says::Window(true)) => Ok(()),
        Ok(Says::Window(false)) => shown_by_no_thread(workspace, true),
        // A name of the user's own is spawned into as it always was; one
        // shaped as a thread's is not taken on trust.
        Err(err) if !names_a_thread(workspace) => {
            eprintln!(
                "warning: could not check whether a thread has the workspace {}: {:#}",
                quoted(workspace),
                err
            );
            Ok(())
        }
        Err(err) => Err(err.context(format!(
            "checking whether a thread has the workspace {}",
            quoted(workspace)
        ))),
    }
}

fn check_in_tree(tree: &codec::ThinkTermTree, workspace: &str) -> anyhow::Result<()> {
    match workspace_owner(tree, workspace) {
        Some((project, thread)) if project.archived_at.is_some() => Err(UsageError(format!(
            "the thread {} of the workspace {} is in the archived project {}, which the \
             sidebar does not show; restore the project there first",
            quoted(&thread.name),
            quoted(workspace),
            quoted(&project.name)
        ))
        .into()),
        Some(_) => Ok(()),
        None => shown_by_no_thread(workspace, false),
    }
}

/// A workspace no thread has: refused when it is shaped as a thread's,
/// spawned into with a warning when it is the user's own name. `open_only`:
/// the window asked can vouch only for the threads it has open.
fn shown_by_no_thread(workspace: &str, open_only: bool) -> anyhow::Result<()> {
    let quoted = quoted(workspace);
    if names_a_thread(workspace) {
        let message = if open_only {
            format!(
                "no thread open in the ThinkTerm window has the workspace {quoted}, so it would \
                 show no window for it; open the thread there first"
            )
        } else {
            format!(
                "no thread has the workspace {quoted}, so ThinkTerm would show no window for \
                 it; --thread NAME puts the program in a thread, made if need be \
                 (`thinkterm cli thread list` shows the threads and their workspaces)"
            )
        };
        return Err(NoSuchTarget(message).into());
    }
    eprintln!(
        "warning: no thread has the workspace {quoted}, so ThinkTerm's sidebar shows no window \
         for it; --thread NAME puts the program in a thread"
    );
    Ok(())
}

/// What every spawn does once its pane is there: label it, print its id,
/// and wait for its program when asked to.
async fn finish(
    client: &Client,
    spawned: codec::SpawnResponse,
    label: Option<String>,
    wait: bool,
    timeout: Option<std::time::Duration>,
) -> anyhow::Result<()> {
    if let Some(label) = label {
        // The pane is there either way, and a script needs its id, and
        // what it ran, more than the label: a program that ended at once
        // can take its pane with it before the label arrives.
        let labelled = client
            .set_pane_label(codec::PaneLabelChanged {
                pane_id: spawned.pane_id,
                label: Some(label),
            })
            .await;
        if let Err(err) = labelled {
            eprintln!("pane {} was not labelled: {err:#}", spawned.pane_id);
        }
    }
    crate::cli::thread::print_out(format!("{}\n", spawned.pane_id).as_bytes())?;
    if wait {
        let deadline = timeout.map(|t| std::time::Instant::now() + t);
        let code = crate::cli::wait::wait_for_program(client, spawned.pane_id, deadline).await?;
        crate::cli::wait::exit_with(code);
    }
    Ok(())
}
