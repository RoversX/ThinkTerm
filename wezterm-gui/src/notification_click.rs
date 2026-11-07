//! A clicked system notification brings up what it was about: the pane
//! whose program sent it, or the thread that finished or wants input.

use crate::scripting::guiwin::GuiWin;
use crate::termwindow::TermWindowNotif;
use mux::pane::PaneId;
use mux::Mux;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};
use wezterm_toast_notification::NotificationClick;
use window::WindowOps;

/// What a notification is about, in the form its click hands back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationTarget {
    Pane(PaneId),
    /// A thread, by its workspace.
    Thread(String),
}

impl NotificationTarget {
    pub fn encode(&self) -> String {
        match self {
            Self::Pane(pane_id) => format!("pane:{}:{pane_id}", this_run()),
            Self::Thread(workspace) => format!("thread:{workspace}"),
        }
    }

    /// `run` is this run's: a pane target from another one is no target.
    fn decode(text: &str, run: &str) -> Option<Self> {
        match text.split_once(':')? {
            ("pane", rest) => {
                let (from, pane_id) = rest.split_once(':')?;
                if from != run {
                    log::debug!("a notification from an earlier run was clicked");
                    return None;
                }
                pane_id.parse().ok().map(Self::Pane)
            }
            ("thread", workspace) if !workspace.is_empty() => {
                Some(Self::Thread(workspace.to_string()))
            }
            _ => None,
        }
    }
}

/// This run of ThinkTerm. Pane ids start over with every run, and the system
/// keeps notifications listed across them: one from before must not bring
/// up whichever pane has its id now. Threads keep their names.
fn this_run() -> &'static str {
    static RUN: OnceLock<String> = OnceLock::new();
    RUN.get_or_init(|| {
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or_default();
        format!("{}-{started}", std::process::id())
    })
}

/// Have clicks on ThinkTerm's notifications bring up their targets.
pub fn install() {
    wezterm_toast_notification::set_click_handler(|click| {
        // The system reports the click on a thread of its own.
        promise::spawn::spawn_into_main_thread(async move { reveal(click) }).detach();
    });
    // The click handler is otherwise set up with this run's first
    // notification, and a click on one still listed from before is lost.
    // Not for someone who turned notifications off: setting it up is what
    // lists ThinkTerm in the system's notification settings.
    #[cfg(target_os = "macos")]
    if crate::native_settings::system_notifications_enabled() {
        wezterm_toast_notification::macos_initialize();
    }
}

fn reveal(click: NotificationClick) {
    let Some(target) = NotificationTarget::decode(&click.target, this_run()) else {
        return;
    };
    log::debug!("bringing up {target:?} for a clicked notification");
    let Some(front_end) = crate::frontend::try_front_end() else {
        return;
    };
    let mux = Mux::get();
    let windows = front_end.gui_windows();
    let token = click.activation_token;
    match target {
        NotificationTarget::Pane(pane_id) => {
            // Closed since; the click has still brought ThinkTerm up.
            let Some((_domain, window_id, _tab_id)) = mux.resolve_pane_id(pane_id) else {
                return;
            };
            if let Some(showing) = windows.iter().find(|gui| gui.mux_window_id == window_id) {
                let window = showing.window.clone();
                showing
                    .window
                    .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                        // From over a page such as Remote Hosts, too.
                        term_window.bring_terminal_forward();
                        term_window.reveal_pane(pane_id);
                        window.invalidate();
                        window.focus_with_activation_token(token.as_deref());
                    })));
            } else if let Some(start) = last_used(&windows) {
                let window = start.window.clone();
                start
                    .window
                    .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                        // Activating puts pages behind the terminal, but
                        // Live Overview has to close.
                        term_window.close_live_overview();
                        if term_window.reveal_pane_elsewhere(pane_id, &window, token.clone()) {
                            window.focus_with_activation_token(token.as_deref());
                        }
                    })));
            }
        }
        NotificationTarget::Thread(workspace) => {
            let showing = windows.iter().find(|gui| {
                mux.get_window(gui.mux_window_id)
                    .is_some_and(|window| window.get_workspace() == workspace)
            });
            if let Some(showing) = showing {
                let window = showing.window.clone();
                showing
                    .window
                    .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                        term_window.bring_terminal_forward();
                        window.invalidate();
                        window.focus_with_activation_token(token.as_deref());
                    })));
            } else if let Some(start) = last_used(&windows) {
                let window = start.window.clone();
                start
                    .window
                    .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                        term_window.close_live_overview();
                        if term_window.reveal_thread(&workspace, &window, token.clone()) {
                            window.focus_with_activation_token(token.as_deref());
                        }
                    })));
            }
        }
    }
}

/// The window last in use, which the paths to other threads and Spaces
/// start from.
fn last_used(windows: &[GuiWin]) -> Option<&GuiWin> {
    let mux = Mux::get();
    let focused = mux
        .active_identity()
        .and_then(|client| mux.resolve_focused_pane(&client))
        .map(|(_domain, window_id, _tab_id, _pane_id)| window_id);
    // A thread's own window goes nowhere else, so it cannot be the start.
    let front_end = crate::frontend::front_end();
    let free = |gui: &&GuiWin| !front_end.is_thread_window(&gui.window);
    windows
        .iter()
        .filter(free)
        .find(|gui| Some(gui.mux_window_id) == focused)
        .or_else(|| windows.iter().find(free))
        .or_else(|| windows.first())
}

#[cfg(test)]
mod tests {
    use super::{this_run, NotificationTarget};

    #[test]
    fn a_target_comes_back_from_the_system_as_it_went() {
        for target in [
            NotificationTarget::Pane(42),
            NotificationTarget::Thread("project:p1:thread-1".to_string()),
        ] {
            assert_eq!(
                NotificationTarget::decode(&target.encode(), this_run()),
                Some(target)
            );
        }
        assert_eq!(NotificationTarget::decode("pane:", this_run()), None);
        assert_eq!(NotificationTarget::decode("thread:", this_run()), None);
        assert_eq!(
            NotificationTarget::decode("https://example.com", this_run()),
            None
        );
    }

    #[test]
    fn a_pane_from_an_earlier_run_is_not_brought_up() {
        let earlier = NotificationTarget::Pane(3).encode();
        assert_eq!(NotificationTarget::decode(&earlier, "1-0"), None);
        // A thread is the same thread in the next run.
        let thread = NotificationTarget::Thread("project:p1:thread-1".to_string());
        assert_eq!(
            NotificationTarget::decode(&thread.encode(), "1-0"),
            Some(thread)
        );
    }
}
