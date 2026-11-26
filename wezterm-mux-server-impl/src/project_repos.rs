//! The git repository each project's folder is in, for a client that asks
//! (`GetProjectRepos`) so it can put the projects of several machines that
//! cloned one repository together.
//!
//! Nothing is read until a client asks, and nothing is kept once none has
//! for a while: the folders are read while they are asked for, and what was
//! read is forgotten after. The files are read on threads of their own -- a
//! folder on a stalled network mount blocks whoever reads it -- and kept per
//! folder, so an answer only ever looks them up. While asked for, the
//! projects' folders are read again every few seconds, which shows a branch
//! switched in a terminal; an answer that changed publishes the session
//! again, and the client asks anew.
//!
//! Only `HEAD` and the config's remote urls are read, and git is never run
//! (`thinkterm_git_info`); a remote goes out without its user, password,
//! port or query, and one on this machine not at all.

use codec::{ProjectRepoEntry, ThinkTermProjectRepo};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// How long an answer stands, and how often the projects' folders are
/// looked at again while asked for.
const KEEP_FOR: Duration = Duration::from_secs(10);
/// How long after a client last asked the folders are still read; then all
/// that was read is forgotten and the watch ends.
const ASKED_FOR: Duration = Duration::from_secs(60);
/// Lookups at once.
const MAX_LOOKUPS: usize = 4;
/// A lookup still running after this is stuck -- on a dead mount, say --
/// and gives its slot up, so stuck folders cannot stop the others being
/// read. Its folder keeps the answer it had and is not looked up again
/// until it returns: one thread at most per folder.
const STUCK_AFTER: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Known {
    repo: Option<ThinkTermProjectRepo>,
    /// When the answer was read, once it has been.
    read_at: Option<Instant>,
    /// When the lookup running now began.
    looking: Option<Instant>,
}

#[derive(Default)]
struct State {
    known: HashMap<PathBuf, Known>,
    /// When a client last asked.
    asked_at: Option<Instant>,
    /// The thread that reads the folders again is running: until no client
    /// has asked for `ASKED_FOR`.
    watching: bool,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);

/// The repository of every project whose folder is in one with a remote on
/// the network, as last read, for a client that asked; and whether some
/// folders are not read yet. Those are read now, and the session is
/// published again once they are.
pub fn answer() -> codec::ProjectRepos {
    let projects = crate::thinkterm_tree::snapshot().projects;
    let Ok(mut state) = STATE.lock() else {
        return codec::ProjectRepos { repos: Vec::new(), reading: true };
    };
    state.asked_at = Some(Instant::now());
    if !state.watching {
        state.watching = start_watching();
    }
    let mut repos = Vec::new();
    let mut reading = false;
    for project in projects.iter().filter(|project| project.archived_at.is_none()) {
        let Some(folder) = folder_of(&project.path) else {
            continue;
        };
        let entry = state.known.entry(folder).or_default();
        // Not read yet, unless stuck: a folder on a dead mount would have
        // the client keep what it had for every project for good.
        reading |= entry.read_at.is_none() && entry.looking.is_none_or(|at| at.elapsed() < STUCK_AFTER);
        // A repository with no remote groups with nothing: not sent.
        if let Some(repo) = entry.repo.clone().filter(|repo| repo.remote.is_some()) {
            repos.push(ProjectRepoEntry {
                project_id: project.id.clone(),
                repo,
            });
        }
    }
    start_stale(&mut state.known);
    codec::ProjectRepos { repos, reading }
}

/// A project's folder as a path on this machine: `~` is the home folder.
fn folder_of(path: &str) -> Option<PathBuf> {
    let path = path.trim();
    if path.is_empty() || path.contains("://") {
        return None;
    }
    Some(if path == "~" {
        config::HOME_DIR.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        config::HOME_DIR.join(rest)
    } else {
        PathBuf::from(path)
    })
}

/// Every few seconds while a client asks: let go of the folders no project
/// has any more, and read the others again. Once none has asked for
/// `ASKED_FOR`, forget everything read and stop. Whether the thread runs.
fn start_watching() -> bool {
    let spawned = std::thread::Builder::new()
        .name("project repos".to_string())
        .spawn(|| loop {
            std::thread::sleep(KEEP_FOR);
            {
                let Ok(mut state) = STATE.lock() else {
                    return;
                };
                if stop_if_unasked(&mut state, Instant::now()) {
                    return;
                }
            }
            let folders: HashSet<PathBuf> = crate::thinkterm_tree::snapshot()
                .projects
                .iter()
                .filter(|project| project.archived_at.is_none())
                .filter_map(|project| folder_of(&project.path))
                .collect();
            let Ok(mut state) = STATE.lock() else {
                return;
            };
            state
                .known
                .retain(|folder, entry| entry.looking.is_some() || folders.contains(folder));
            start_stale(&mut state.known);
        });
    match spawned {
        Ok(_) => true,
        Err(err) => {
            log::warn!("project repos: cannot watch: {err:#}");
            false
        }
    }
}

/// Once no client has asked for `ASKED_FOR`: forget everything read and
/// end the watch; whether it ended. A lookup still running keeps its
/// folder's place, so a stuck one is not started twice, but nothing it
/// read stays (`look_up`).
fn stop_if_unasked(state: &mut State, now: Instant) -> bool {
    if state.asked_at.is_some_and(|at| now.duration_since(at) < ASKED_FOR) {
        return false;
    }
    state.known.retain(|_, entry| entry.looking.is_some());
    for entry in state.known.values_mut() {
        entry.repo = None;
        entry.read_at = None;
    }
    state.asked_at = None;
    state.watching = false;
    true
}

/// Start lookups for the folders whose answer is due, as slots allow: a
/// slot is a lookup running now that is not stuck.
fn start_stale(known: &mut HashMap<PathBuf, Known>) {
    let now = Instant::now();
    let mut busy = known
        .values()
        .filter(|entry| entry.looking.is_some_and(|at| now.duration_since(at) < STUCK_AFTER))
        .count();
    let due: Vec<PathBuf> = known
        .iter()
        .filter(|(_, entry)| {
            entry.looking.is_none()
                && entry
                    .read_at
                    .is_none_or(|at| now.duration_since(at) >= KEEP_FOR)
        })
        .map(|(folder, _)| folder.clone())
        .collect();
    for folder in due {
        if busy >= MAX_LOOKUPS {
            break;
        }
        let Some(entry) = known.get_mut(&folder) else {
            continue;
        };
        let spawned = std::thread::Builder::new()
            .name("project repo lookup".to_string())
            .spawn({
                let folder = folder.clone();
                move || look_up(folder)
            });
        if spawned.is_ok() {
            entry.looking = Some(now);
            busy += 1;
        }
    }
}

fn look_up(folder: PathBuf) {
    let repo = thinkterm_git_info::checkout_of(&folder).map(|checkout| ThinkTermProjectRepo {
        worktree: checkout
            .linked
            .then(|| checkout.folder.file_name())
            .flatten()
            .map(|name| name.to_string_lossy().into_owned()),
        remote: checkout.remote,
        branch: checkout.branch,
    });
    let changed = match STATE.lock() {
        Ok(mut state) => {
            let watching = state.watching;
            let changed = match state.known.get_mut(&folder) {
                // Still asked for: kept, and published when it changed.
                Some(entry) if watching => {
                    let changed = entry.repo != repo;
                    entry.repo = repo;
                    entry.read_at = Some(Instant::now());
                    entry.looking = None;
                    changed
                }
                _ => false,
            };
            if watching {
                start_stale(&mut state.known);
            } else {
                // Nobody asks any more: what this read is not kept.
                state.known.remove(&folder);
            }
            changed
        }
        Err(_) => false,
    };
    if changed {
        crate::thinkterm_session::publish_changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(remote: &str) -> ThinkTermProjectRepo {
        ThinkTermProjectRepo {
            remote: Some(remote.into()),
            branch: Some("main".into()),
            worktree: None,
        }
    }

    #[test]
    fn what_was_read_is_forgotten_once_nobody_asks() {
        let now = Instant::now();
        let mut state = State::default();
        state.watching = true;
        state.asked_at = Some(now);
        state.known.insert(
            "/read".into(),
            Known { repo: Some(repo("example.com/a/b")), read_at: Some(now), looking: None },
        );
        state.known.insert(
            "/stuck".into(),
            Known { repo: Some(repo("example.com/c/d")), read_at: Some(now), looking: Some(now) },
        );
        assert!(!stop_if_unasked(&mut state, now + ASKED_FOR / 2), "still asked for");
        assert_eq!(state.known.len(), 2);

        assert!(stop_if_unasked(&mut state, now + ASKED_FOR));
        assert!(!state.watching);
        assert_eq!(state.asked_at, None);
        // The stuck lookup keeps its place, and nothing else.
        let left: Vec<_> = state.known.iter().map(|(f, e)| (f.clone(), e.repo.clone(), e.read_at)).collect();
        assert_eq!(left, [(PathBuf::from("/stuck"), None, None)]);
    }

    #[test]
    fn a_lookup_is_kept_only_while_asked_for() {
        let dir = std::env::temp_dir().join(format!("tt-project-repos-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            dir.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://user:secret@example.com/owner/repo.git\n",
        )
        .unwrap();
        let read = repo("example.com/owner/repo");
        let looking = || Known { repo: Some(read.clone()), read_at: None, looking: Some(Instant::now()) };

        // Asked for: kept, without the user or the secret. The answer is
        // the one known already, so nothing is published.
        {
            let mut state = STATE.lock().unwrap();
            state.watching = true;
            state.known.insert(dir.clone(), looking());
        }
        look_up(dir.clone());
        {
            let state = STATE.lock().unwrap();
            let entry = &state.known[&dir];
            assert_eq!(entry.repo, Some(read.clone()));
            assert!(entry.read_at.is_some() && entry.looking.is_none());
        }

        // The watch ended while it was reading: nothing it read stays.
        {
            let mut state = STATE.lock().unwrap();
            state.watching = false;
            state.known.insert(dir.clone(), looking());
        }
        look_up(dir.clone());
        assert!(!STATE.lock().unwrap().known.contains_key(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
