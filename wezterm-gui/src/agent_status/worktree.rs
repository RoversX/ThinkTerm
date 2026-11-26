//! The linked git worktree an agent's pane is in, for the Agents panel: the
//! folder it is checked out in and the branch there. A repository's own
//! checkout shows nothing, since the thread already names its project.
//! Read from the files git keeps rather than by running git, on a thread of
//! its own -- a directory on a stalled network mount, or a file not yet
//! downloaded, blocks whoever reads it -- and kept per directory, so the
//! panel only ever looks it up.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Worktree {
    /// The folder it is checked out in, by name.
    pub folder: String,
    /// The branch checked out, or the commit when none is.
    pub branch: Option<String>,
}

impl Worktree {
    /// The line the panel shows: "folder · branch".
    pub fn label(&self) -> String {
        match &self.branch {
            Some(branch) => format!("{} \u{b7} {}", self.folder, branch),
            None => self.folder.clone(),
        }
    }
}

/// How long an answer stands before it is looked up again; a branch
/// switched meanwhile shows after about this.
const KEEP_FOR: Duration = Duration::from_secs(5);
/// How many directories are remembered, the one asked about least recently
/// going first: agents sit in far fewer.
const MAX_DIRECTORIES: usize = 64;
/// Lookups at once. One stuck on a dead mount keeps its slot -- and its
/// directory the answer it had -- for good, so a few such directories
/// cannot pile up threads.
const MAX_LOOKUPS: usize = 4;
/// How long this machine's names are reused: a Mac's can follow the
/// network it is on.
const NAMES_KEEP_FOR: Duration = Duration::from_secs(30);

struct Known {
    worktree: Option<Worktree>,
    /// When the answer was read, once it has been.
    read_at: Option<Instant>,
    /// When the panel last asked, which decides what is forgotten first.
    asked_at: Instant,
    looking: bool,
    /// Asked while every lookup slot was taken. It goes when one frees: a
    /// panel with nothing moving does not ask again.
    waiting: bool,
}

static KNOWN: LazyLock<Mutex<HashMap<PathBuf, Known>>> = LazyLock::new(Default::default);
static LOOKUPS: AtomicUsize = AtomicUsize::new(0);

/// The worktree a pane whose directory is `url` is in, as last looked up:
/// `None` until it has been, or when the directory is on another machine
/// or in no linked worktree. A stale answer has a fresh lookup started.
pub(crate) fn worktree_of(url: &url::Url) -> Option<Worktree> {
    let dir = local_dir(url, &this_machine_names())?;
    let now = Instant::now();
    let mut known = KNOWN.lock().ok()?;
    let new = !known.contains_key(&dir);
    let entry = known.entry(dir.clone()).or_insert_with(|| Known {
        worktree: None,
        read_at: None,
        asked_at: now,
        looking: false,
        waiting: false,
    });
    entry.asked_at = now;
    let stale = entry
        .read_at
        .is_none_or(|at| now.duration_since(at) >= KEEP_FOR);
    let needs_lookup = stale && !entry.looking;
    let worktree = entry.worktree.clone();
    if needs_lookup {
        if LOOKUPS.load(Ordering::Relaxed) < MAX_LOOKUPS {
            start_lookup(&mut known, &dir);
        } else if let Some(entry) = known.get_mut(&dir) {
            entry.waiting = true;
        }
    }
    if new {
        forget_least_asked(&mut known);
    }
    worktree
}

/// Look `dir` up on a thread of its own, in a slot. The slots are counted
/// with the map's lock held, as here.
fn start_lookup(known: &mut HashMap<PathBuf, Known>, dir: &Path) {
    let Some(entry) = known.get_mut(dir) else {
        return;
    };
    entry.looking = true;
    entry.waiting = false;
    LOOKUPS.fetch_add(1, Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name("worktree lookup".to_string())
        .spawn({
            let dir = dir.to_path_buf();
            move || look_up(dir)
        });
    if spawned.is_err() {
        entry.looking = false;
        LOOKUPS.fetch_sub(1, Ordering::Relaxed);
    }
}

fn look_up(dir: PathBuf) {
    let worktree = read_worktree(&dir);
    let changed = match KNOWN.lock() {
        Ok(mut known) => {
            // A directory being looked up is never forgotten.
            let changed = match known.get_mut(&dir) {
                Some(entry) => {
                    let changed = entry.worktree != worktree;
                    entry.worktree = worktree;
                    // Stamped once read: a read that took longer than
                    // KEEP_FOR was otherwise stale on arrival, and looked up
                    // again.
                    entry.read_at = Some(Instant::now());
                    entry.looking = false;
                    changed
                }
                None => false,
            };
            LOOKUPS.fetch_sub(1, Ordering::Relaxed);
            if let Some(next) = next_waiting(&known) {
                start_lookup(&mut known, &next);
            }
            changed
        }
        Err(_) => {
            LOOKUPS.fetch_sub(1, Ordering::Relaxed);
            false
        }
    };
    if changed {
        promise::spawn::spawn_into_main_thread(async {
            // There, not here: a snapshot the GUI was building meanwhile
            // would be stored after this, with the old answer in it.
            super::invalidate_agent_pane_cache();
            if let Some(front_end) = crate::frontend::try_front_end() {
                front_end.invalidate_all_windows();
            }
        })
        .detach();
    }
}

/// The directory a freed slot goes to: of those that found none free, the
/// one asked about last.
fn next_waiting(known: &HashMap<PathBuf, Known>) -> Option<PathBuf> {
    known
        .iter()
        .filter(|(_, entry)| entry.waiting && !entry.looking)
        .max_by_key(|(_, entry)| entry.asked_at)
        .map(|(dir, _)| dir.clone())
}

/// Keep to the bound: the directory asked about least recently goes, never
/// one being looked up.
fn forget_least_asked(known: &mut HashMap<PathBuf, Known>) {
    while known.len() > MAX_DIRECTORIES {
        let least = known
            .iter()
            .filter(|(_, entry)| !entry.looking)
            .min_by_key(|(_, entry)| entry.asked_at)
            .map(|(dir, _)| dir.clone());
        match least {
            Some(dir) => known.remove(&dir),
            None => break,
        };
    }
}

/// This machine's names as a shell may report them: its host name, and on
/// Windows the computer name PowerShell's `$env:COMPUTERNAME` gives, which
/// is cut to 15 characters.
fn this_machine_names() -> Vec<String> {
    static NAMES: Mutex<Option<(Instant, Vec<String>)>> = Mutex::new(None);
    let now = Instant::now();
    let mut cached = match NAMES.lock() {
        Ok(cached) => cached,
        Err(_) => return read_machine_names(),
    };
    if let Some((at, names)) = cached.as_ref() {
        if now.duration_since(*at) < NAMES_KEEP_FOR {
            return names.clone();
        }
    }
    let names = read_machine_names();
    *cached = Some((now, names.clone()));
    names
}

fn read_machine_names() -> Vec<String> {
    let mut names: Vec<String> = hostname::get()
        .ok()
        .map(|name| name.to_string_lossy().into_owned())
        .into_iter()
        .collect();
    if cfg!(windows) {
        names.extend(std::env::var("COMPUTERNAME").ok());
    }
    names.retain(|name| !name.is_empty());
    names
}

/// Whether a shell's report of the machine it runs on -- a `file:` URL's
/// host, the `WEZTERM_HOST` user var -- names this one.
pub(crate) fn names_this_machine(host: &str) -> bool {
    is_this_machine(host, &this_machine_names())
}

/// Whether `host` is this machine: no name, `localhost`, or one of
/// `names`. A shell reports its directory with the name of the machine it
/// runs on, which after an `ssh` is the other one.
fn is_this_machine(host: &str, names: &[String]) -> bool {
    host.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || names.iter().any(|name| same_machine(host, name))
}

/// A name and the same name with a domain (`myhost`, `myhost.local`) are
/// one machine; the same first part under two domains is two.
fn same_machine(a: &str, b: &str) -> bool {
    if a.eq_ignore_ascii_case(b) {
        return true;
    }
    match (a.split_once('.'), b.split_once('.')) {
        (Some((short, _)), None) => short.eq_ignore_ascii_case(b),
        (None, Some((short, _))) => a.eq_ignore_ascii_case(short),
        _ => false,
    }
}

/// The path a pane's directory `url` names on this machine.
fn local_dir(url: &url::Url, names: &[String]) -> Option<PathBuf> {
    if url.scheme() != "file" || !is_this_machine(url.host_str().unwrap_or(""), names) {
        return None;
    }
    let path = percent_encoding::percent_decode_str(url.path())
        .decode_utf8()
        .ok()?;
    // `/C:/Users/...` on Windows.
    let path = match path.strip_prefix('/') {
        Some(rest) if cfg!(windows) && rest.as_bytes().get(1) == Some(&b':') => rest,
        _ => &path,
    };
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// The linked worktree `dir` is in, when the checkout it is in is one: a
/// repository's own checkout, a submodule or a separated git directory is
/// not.
fn read_worktree(dir: &Path) -> Option<Worktree> {
    let checkout = thinkterm_git_info::checkout_of(dir)?;
    if !checkout.linked {
        return None;
    }
    Some(Worktree {
        folder: checkout.folder.file_name()?.to_string_lossy().into_owned(),
        branch: checkout.branch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_machine_s_directories_are_read() {
        let url = |text: &str| url::Url::parse(text).unwrap();
        let here = ["myhost.local".to_string()];
        assert_eq!(
            local_dir(&url("file:///home/user/project"), &here),
            Some(PathBuf::from("/home/user/project"))
        );
        assert!(local_dir(&url("file://localhost/home/user/project"), &here).is_some());
        assert!(local_dir(&url("file://MYHOST/home/user/project"), &here).is_some());
        assert_eq!(
            local_dir(&url("file://myhost.local/home/user/a%20b"), &here),
            Some(PathBuf::from("/home/user/a b"))
        );
        // After `ssh server`, the shell there names its own machine.
        assert_eq!(
            local_dir(&url("file://server/home/user/project"), &here),
            None
        );
        assert_eq!(local_dir(&url("https://example.com/x"), &here), None);
    }

    #[test]
    fn the_same_first_name_under_two_domains_is_two_machines() {
        let here = ["myhost.local.example".to_string()];
        assert!(is_this_machine("myhost.local.example", &here));
        assert!(is_this_machine("MYHOST", &here));
        assert!(!is_this_machine("myhost.remote.example", &here));
        // PowerShell gives the computer name, the host name's first 15
        // characters.
        let windows = [
            "myhost-workstation".to_string(),
            "MYHOST-WORKSTAT".to_string(),
        ];
        assert!(is_this_machine("MYHOST-WORKSTAT", &windows));
        assert!(!is_this_machine("server", &windows));
    }

    fn entry(asked_at: Instant, looking: bool, waiting: bool) -> Known {
        Known {
            worktree: None,
            read_at: None,
            asked_at,
            looking,
            waiting,
        }
    }

    #[test]
    fn a_freed_slot_goes_to_the_directory_asked_about_last() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut known = HashMap::new();
        known.insert(PathBuf::from("/early"), entry(at(1), false, true));
        known.insert(PathBuf::from("/late"), entry(at(3), false, true));
        known.insert(PathBuf::from("/busy"), entry(at(4), true, false));
        known.insert(PathBuf::from("/done"), entry(at(5), false, false));
        assert_eq!(next_waiting(&known), Some(PathBuf::from("/late")));
        known.clear();
        assert_eq!(next_waiting(&known), None);
    }

    #[test]
    fn the_directory_asked_about_least_recently_goes_first() {
        let start = Instant::now();
        let mut known: HashMap<PathBuf, Known> = (0..=MAX_DIRECTORIES)
            .map(|at| {
                let asked_at = start + Duration::from_millis(at as u64 + 1);
                (
                    PathBuf::from(format!("/dir/{at}")),
                    entry(asked_at, false, false),
                )
            })
            .collect();
        // The oldest of all, but still being looked up.
        known.insert(PathBuf::from("/stuck"), entry(start, true, false));
        forget_least_asked(&mut known);
        assert_eq!(known.len(), MAX_DIRECTORIES);
        assert!(known.contains_key(Path::new("/stuck")));
        assert!(!known.contains_key(Path::new("/dir/0")));
        assert!(!known.contains_key(Path::new("/dir/1")));
        assert!(known.contains_key(Path::new("/dir/2")));
    }

    #[test]
    fn only_a_linked_worktree_names_its_folder() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("project");
        std::fs::create_dir_all(main.join(".git/worktrees/project-fix")).unwrap();
        std::fs::write(main.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            main.join(".git/worktrees/project-fix/HEAD"),
            "ref: refs/heads/fix\n",
        )
        .unwrap();
        std::fs::write(main.join(".git/worktrees/project-fix/commondir"), "../..\n").unwrap();
        // A submodule's git directory has a HEAD but no commondir.
        std::fs::create_dir_all(main.join(".git/modules/lib")).unwrap();
        std::fs::write(main.join(".git/modules/lib/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(main.join("lib")).unwrap();
        std::fs::write(main.join("lib/.git"), "gitdir: ../.git/modules/lib\n").unwrap();
        let linked = root.path().join("project-fix");
        std::fs::create_dir_all(linked.join("src")).unwrap();
        // As `git worktree add --relative-paths` writes it.
        std::fs::write(
            linked.join(".git"),
            "gitdir: ../project/.git/worktrees/project-fix\r\n",
        )
        .unwrap();
        std::fs::create_dir_all(main.join("src/deep")).unwrap();

        assert_eq!(read_worktree(&main.join("src/deep")), None);
        assert_eq!(read_worktree(&main.join("lib")), None);
        let worktree = read_worktree(&linked.join("src")).unwrap();
        assert_eq!(worktree.label(), "project-fix \u{b7} fix");
    }
}
