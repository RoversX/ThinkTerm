//! What `thinkterm start` names, a folder to open and a program to run,
//! opened in the window's current Space: the folder as a project (the one
//! the Space has for it, else a new one) and the program in a thread of its
//! own (`workspace_threads::thread_for_start`).
//!
//! A launch that hands it to the window already running sends it over
//! (`OpenFromStart`). One that starts the window keeps it here, and the
//! first window opens it in place of the thread it would have restored:
//! the startup tab is no place for it, since a restore that finds the
//! Space's terminals already running never opens that tab.

use crate::termwindow::{TermWindow, TermWindowNotif};
use mux::command_spec::CommandSpecExt;
use portable_pty::CommandBuilder;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use wezterm_gui_subcommands::StartCommand;
use window::WindowOps as _;

#[derive(Clone, Debug)]
pub(crate) struct StartRequest {
    /// An absolute path to a directory, as the launch resolved it.
    pub folder: Option<String>,
    pub program: Option<CommandBuilder>,
}

impl StartRequest {
    /// What `opts` names, when it names a folder or a program and nothing
    /// that puts them elsewhere: a domain, a workspace, or a tab of the
    /// window already running. A folder that is not one is an error.
    pub(crate) fn from_start(opts: &StartCommand) -> anyhow::Result<Option<Self>> {
        if opts.domain.is_some() || opts.workspace.is_some() || opts.new_tab || opts.attach {
            return Ok(None);
        }
        let folder = match &opts.cwd {
            Some(cwd) if cwd.is_relative() => Some(std::env::current_dir()?.join(cwd)),
            Some(cwd) => Some(cwd.clone()),
            None => None,
        };
        let program = (!opts.prog.is_empty()).then(|| CommandBuilder::from_argv(opts.prog.clone()));
        if folder.is_none() && program.is_none() {
            return Ok(None);
        }
        let folder = match folder {
            Some(folder) => {
                anyhow::ensure!(folder.is_dir(), "{} is not a folder", folder.display());
                Some(folder.into_os_string().into_string().map_err(|path| {
                    anyhow::anyhow!("the folder {path:?} is not valid Unicode")
                })?)
            }
            None => None,
        };
        Ok(Some(Self { folder, program }))
    }

    pub(crate) fn to_pdu(&self) -> codec::OpenFromStart {
        codec::OpenFromStart {
            folder: self.folder.clone(),
            command: self
                .program
                .as_ref()
                .map(thinkterm_proto::CommandSpec::from_command_builder),
        }
    }

    fn from_pdu(pdu: codec::OpenFromStart) -> Self {
        Self {
            folder: pdu.folder,
            program: pdu.command.map(|spec| spec.into_command_builder()),
        }
    }
}

/// Requests for the first window to open, oldest first: this launch's, and
/// any handed over before a window was up.
static PENDING: Mutex<Vec<StartRequest>> = Mutex::new(Vec::new());

/// Keep `request` for the first window this launch opens.
pub(crate) fn keep_for_first_window(request: StartRequest) {
    PENDING.lock().unwrap().push(request);
}

/// The requests kept for the first window, taken.
pub(crate) fn take_for_window() -> Vec<StartRequest> {
    std::mem::take(&mut *PENDING.lock().unwrap())
}

/// `OpenFromStart`, as the server this process runs for the command line
/// hands it over, on the main thread: opened by the window last in front,
/// else by a window opened for it.
pub(crate) fn open_handed(pdu: codec::OpenFromStart) -> anyhow::Result<()> {
    let request = StartRequest::from_pdu(pdu);
    let front_end =
        crate::frontend::try_front_end().ok_or_else(|| anyhow::anyhow!("no window is open"))?;
    if !front_end.gui_windows().is_empty() {
        hand_to_a_window(request);
    } else if !front_end.startup_has_settled() {
        // The first window is on its way, and opens it.
        keep_for_first_window(request);
    } else {
        front_end.spawn_space_window_then(move || hand_to_a_window(request));
    }
    Ok(())
}

fn hand_to_a_window(request: StartRequest) {
    let Some(front_end) = crate::frontend::try_front_end() else {
        return;
    };
    let windows = front_end.gui_windows();
    // The window in front, or last in front (focus goes to the file manager
    // that asks); else, or when that one has closed, the last to hear.
    let preferred = crate::termwindow::last_focused_window_owner();
    let claimed = Arc::new(AtomicBool::new(false));
    let unheard = Arc::new(AtomicUsize::new(windows.len()));
    for gui in windows {
        let request = request.clone();
        let claimed = Arc::clone(&claimed);
        let unheard = Arc::clone(&unheard);
        gui.window.notify(TermWindowNotif::Apply(Box::new(
            move |tw: &mut TermWindow| {
                let last = unheard.fetch_sub(1, Ordering::SeqCst) == 1;
                let wanted = preferred.map_or(true, |owner| tw.is_space_owner(owner));
                if (wanted || last) && !claimed.swap(true, Ordering::SeqCst) {
                    tw.open_from_start(request);
                }
            },
        )));
    }
}

/// The thread `thread_id` of the Space `space_id`, made for what `thinkterm
/// start` named, opened by the window that shows that Space: the window
/// handed the request could not move to it.
pub(crate) fn hand_thread_to_its_space(space_id: String, thread_id: String) {
    let Some(front_end) = crate::frontend::try_front_end() else {
        return;
    };
    let claimed = Arc::new(AtomicBool::new(false));
    for gui in front_end.gui_windows() {
        let (space_id, thread_id) = (space_id.clone(), thread_id.clone());
        let claimed = Arc::clone(&claimed);
        gui.window.notify(TermWindowNotif::Apply(Box::new(
            move |tw: &mut TermWindow| {
                if tw.active_space_id() == space_id && !claimed.swap(true, Ordering::SeqCst) {
                    tw.open_started_thread(space_id, thread_id);
                }
            },
        )));
    }
}
