//! What first-run setup offers to bring in from this computer: another
//! program's terminal sessions, and a WezTerm configuration.
//!
//! What is on disk is read when the page opens, so the page has its final
//! height from the first frame. Whether a session is running means asking
//! its program over a socket, which can take seconds when that program is
//! wedged, so that runs on a thread and the row's wording catches up.

use crate::termwindow::ui::icons::BrandIcon;
use std::sync::{Arc, Mutex};

/// One program whose sessions Settings can import. Session import is
/// Unix-only, so elsewhere there are none.
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) struct FoundSessions {
    /// The import source's id, which the Import page is opened on.
    pub id: &'static str,
    pub name: &'static str,
    pub icon: Option<BrandIcon>,
    pub count: usize,
    running: Arc<Mutex<Option<usize>>>,
}

impl FoundSessions {
    /// How many of them are running, once the check has asked each one.
    pub fn running(&self) -> Option<usize> {
        *self.running.lock().unwrap()
    }
}

#[derive(Default)]
pub(super) struct Found {
    pub sessions: Vec<FoundSessions>,
    pub wezterm_config: bool,
}

impl Found {
    pub fn look() -> Self {
        Self {
            sessions: sessions(),
            wezterm_config: crate::settings_window::wezterm_config_found(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty() && !self.wezterm_config
    }

    /// Whether a running check is still out.
    pub fn pending(&self) -> bool {
        self.sessions.iter().any(|found| found.running().is_none())
    }
}

#[cfg(unix)]
fn sessions() -> Vec<FoundSessions> {
    let Ok(context) = wezterm_mux_server_impl::session_import::context() else {
        return Vec::new();
    };
    thinkterm_import::sources()
        .into_iter()
        .filter_map(|info| {
            let source = thinkterm_import::source(info.id).ok()?;
            let names: Vec<String> = source
                .discover(&context)
                .ok()?
                .into_iter()
                .map(|session| session.name)
                .collect();
            let count = names.len();
            if count == 0 {
                return None;
            }
            let running = Arc::new(Mutex::new(None));
            let answer = Arc::clone(&running);
            let context = context.clone();
            let spawned = std::thread::Builder::new()
                .name("onboarding-sessions".into())
                .spawn(move || {
                    let live = count_running(&names, |name| {
                        source
                            .preview(&context, name)
                            .is_ok_and(|preview| preview.live)
                    });
                    *answer.lock().unwrap() = Some(live);
                });
            if spawned.is_err() {
                // Without the check the row still says how many there are.
                *running.lock().unwrap() = Some(0);
            }
            Some(FoundSessions {
                id: info.id,
                name: info.name,
                icon: BrandIcon::for_import_source(info.icon),
                count,
                running,
            })
        })
        .collect()
}

/// How many of `names` `live` says are running. A source that panics on one
/// (a malformed session, say) still gets an answer, none running, so the
/// page stops polling for it.
#[cfg(unix)]
fn count_running(names: &[String], live: impl Fn(&str) -> bool) -> usize {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        names.iter().filter(|name| live(name)).count()
    }))
    .unwrap_or(0)
}

#[cfg(not(unix))]
fn sessions() -> Vec<FoundSessions> {
    Vec::new()
}

#[cfg(test)]
impl FoundSessions {
    pub fn for_test(id: &'static str, count: usize, running: Option<usize>) -> Self {
        Self {
            id,
            name: "Example",
            icon: None,
            count,
            running: Arc::new(Mutex::new(running)),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_check_that_panics_still_answers() {
        let names = vec!["default".to_string(), "work".to_string()];
        assert_eq!(count_running(&names, |name| name == "work"), 1);
        assert_eq!(count_running(&names, |_| panic!("a malformed session")), 0);
    }
}
