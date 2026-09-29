//! The fetcher: git asked, on a thread of its own, about the directories the
//! panels on show are in -- every [`EVERY`], or less often in a repository
//! git takes a while over, and at once when a panel comes or moves -- and
//! the panels drawn again when what it says changed. A file picked has its
//! diff alone read at once, and none read when one is kept from before.
//! Nothing is asked while no panel is on show. A directory on another
//! machine is looked at there, through ThinkTerm ([`There`]).

use crate::git::{
    self, Change, Counted, Failure, FileDiff, Found, Machine, Meta, Snapshot, Status,
};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use thinkterm_plugin_sdk::panel::{Answer, Ask, Env, Remote};
use thinkterm_plugin_sdk::Emitter;

/// How often git is asked again while a panel is on show.
pub const EVERY: Duration = Duration::from_secs(2);
/// How long ThinkTerm is given to answer for another machine: git's own
/// patience there, and the trip.
const ASK_WAIT: Duration = Duration::from_secs(30);
/// How many directories no panel is in any more keep what was found there.
const KEPT: usize = 4;
/// How many files' diffs a directory keeps once no panel shows them, and
/// how much of what git printed for them: a file picked again is shown at
/// once, while the list has it as it had it.
const KEPT_DIFFS: usize = 32;
const KEPT_DIFF_BYTES: usize = 4 * 1024 * 1024;

pub type Shared = Arc<(Mutex<State>, Condvar)>;

/// A directory a terminal beside a panel is in: on this machine, or on the
/// other one `host` is.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Dir {
    pub host: Option<Host>,
    pub path: String,
}

/// Another machine, as ThinkTerm names it: `name` to show, and `machine`
/// to ask it by, which tells two accounts on one host apart.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Host {
    pub name: String,
    pub machine: String,
}

impl Dir {
    /// Where the terminal beside a panel of `env` is, if it says.
    pub fn of(env: &Env) -> Option<Self> {
        match (&env.cwd, &env.remote) {
            (Some(cwd), _) => Some(Self {
                host: None,
                path: cwd.clone(),
            }),
            (None, Some(Remote { host, machine, cwd })) => Some(Self {
                host: Some(Host {
                    name: host.clone(),
                    machine: machine.clone(),
                }),
                path: cwd.clone(),
            }),
            (None, None) => None,
        }
    }

    /// What the panel's lists here are kept by: one for each place, which
    /// two accounts on one host are too.
    pub fn key(&self) -> String {
        match &self.host {
            Some(host) => format!("{}:{}", host.machine, self.path),
            None => self.path.clone(),
        }
    }
}

impl fmt::Display for Dir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Some(host) => write!(f, "{}:{}", host.name, self.path),
            None => f.write_str(&self.path),
        }
    }
}

#[derive(Default)]
pub struct State {
    /// What each panel on show wants, by its view: its extended view goes
    /// by the panel's.
    pub views: HashMap<u64, Want>,
    /// What was found in each directory a panel is in, and in those in
    /// [`State::left`].
    pub places: HashMap<Dir, Place>,
    /// The directories panels left last, the last last, at most [`KEPT`]:
    /// what was found there is kept for as long as the program runs, so
    /// that a panel back in one -- beside another tab's terminal, or shown
    /// again after the sidebar went to another of its panels or closed --
    /// shows it at once while it is looked at again, rather than looking
    /// afresh. The program itself stops once unused for long enough.
    left: Vec<Dir>,
    /// The file picked last in each directory of [`State::places`]: a
    /// panel back in one picks it again.
    picked_in: HashMap<Dir, String>,
    /// A look is wanted at once.
    poked: bool,
    /// A panel picked a file: its diff is wanted at once, the repository
    /// not looked at again for it.
    picking: bool,
    /// The fetcher was handed something to look at since the last panel
    /// went: what it keeps for that is still to be let go.
    looking: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Want {
    /// The directory of the terminal beside the panel.
    pub dir: Option<Dir>,
    /// The file it picked, from the repository's root: shown in its
    /// extended view.
    pub picked: Option<String>,
    /// It shows a file's lines itself, below the files -- the first file's
    /// while none is picked -- with no extended view to show them in.
    pub inside: bool,
}

pub enum Place {
    NoGit,
    NotRepo,
    Failed(String),
    Repo(Repo),
}

pub struct Repo {
    pub snapshot: Snapshot,
    /// Changes whenever the files listed do.
    pub revision: u32,
    /// The diffs of the files the panels show, by path, as last read, and
    /// of some shown before ([`KEPT_DIFFS`]).
    pub diffs: HashMap<String, Read>,
}

#[derive(Clone)]
pub struct Read {
    pub diff: Arc<FileDiff>,
    /// Changes whenever the diff does.
    pub version: u32,
    /// What git printed, hashed: the same again is the same diff.
    raw: u64,
    /// The file as the list had it when this was read: listed otherwise
    /// since, it changed, and this is not its diff any more.
    of: Change,
    /// How much git printed: what keeping it costs.
    bytes: usize,
    /// When it was last read for a panel to show: those shown longest ago
    /// are let go first.
    shown: Instant,
}

impl State {
    /// Lets go of what was found in the directories no panel is in, but for
    /// the [`KEPT`] left last.
    fn keep_places(&mut self) {
        let State {
            views,
            places,
            left,
            picked_in,
            ..
        } = self;
        let wanted = |dir: &Dir| views.values().any(|want| want.dir.as_ref() == Some(dir));
        left.retain(|dir| !wanted(dir));
        for dir in places.keys() {
            if !wanted(dir) && !left.contains(dir) {
                left.push(dir.clone());
            }
        }
        let over = left.len().saturating_sub(KEPT);
        left.drain(..over);
        places.retain(|dir, _| wanted(dir) || left.contains(dir));
        picked_in.retain(|dir, _| places.contains_key(dir));
    }

    /// The file panel `panel` picked in `repo`, while that is changed still.
    pub fn picked<'a>(&self, panel: u64, repo: &'a Repo) -> Option<&'a Change> {
        let path = self.views.get(&panel)?.picked.as_ref()?;
        repo.snapshot.files.iter().find(|file| &file.path == path)
    }

    /// The file whose lines panel `panel` shows, in itself or its extended
    /// view: the one it picked, or the first of `repo`'s when it shows
    /// them itself.
    pub fn shown<'a>(&self, panel: u64, repo: &'a Repo) -> Option<&'a Change> {
        let want = self.views.get(&panel)?;
        let first = want.inside.then(|| repo.snapshot.files.first()).flatten();
        self.picked(panel, repo).or(first)
    }
}

impl Read {
    #[cfg(test)]
    pub fn new(of: Change, diff: FileDiff, version: u32) -> Self {
        Self {
            diff: Arc::new(diff),
            version,
            raw: 0,
            of,
            bytes: 0,
            shown: Instant::now(),
        }
    }
}

pub fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Panel `panel` is beside a terminal in `dir`, and shows a file's lines
/// itself or not: looked at at once when either is news. A panel that
/// moves picks what was picked last where it is now, if anything was.
pub fn place(shared: &Shared, panel: u64, dir: Option<Dir>, inside: bool) {
    let mut state = lock(shared);
    let State {
        views, picked_in, ..
    } = &mut *state;
    let want = views.entry(panel).or_default();
    if want.dir != dir || want.inside != inside {
        let moved = want.dir != dir;
        if moved {
            want.picked = dir.as_ref().and_then(|dir| picked_in.get(dir).cloned());
        }
        want.dir = dir;
        want.inside = inside;
        // The directory left is kept as it is left, in turn.
        if moved {
            state.keep_places();
        }
        state.poked = true;
        shared.1.notify_all();
    }
}

/// Panel `panel` picked the file at `path`, or put it down: its diff read
/// at once unless it was read before, and picked again when a panel comes
/// back to the directory.
pub fn pick(shared: &Shared, panel: u64, path: Option<String>) {
    let mut state = lock(shared);
    let State {
        views,
        picked_in,
        picking,
        ..
    } = &mut *state;
    let want = views.entry(panel).or_default();
    if want.picked != path {
        if let Some(dir) = &want.dir {
            match &path {
                Some(path) => picked_in.insert(dir.clone(), path.clone()),
                None => picked_in.remove(dir),
            };
        }
        want.picked = path;
        *picking = true;
        shared.1.notify_all();
    }
}

/// View `view` went: what was found for it alone is let go now, and what
/// the fetcher keeps for it on its next turn.
pub fn close(shared: &Shared, view: u64) {
    let mut state = lock(shared);
    state.views.remove(&view);
    state.keep_places();
    shared.1.notify_all();
}

/// Another machine, `machine`, reached through ThinkTerm: what is asked of
/// it goes on panel `view`'s behalf, one of the panels beside a terminal
/// there.
pub struct There {
    pub emitter: Emitter,
    pub view: u64,
    pub machine: String,
}

impl There {
    fn ask(&self, ask: &Ask) -> Answer {
        self.emitter.ask(self.view, &self.machine, ask, ASK_WAIT)
    }
}

impl Machine for There {
    fn git(&self, dir: &str, args: &[&str], limit: usize) -> Result<(Vec<u8>, bool), Failure> {
        let run: Vec<String> = ["env", "GIT_TERMINAL_PROMPT=0", "git"]
            .iter()
            .chain(git::GIT_OPTIONS.iter())
            .chain(["-C", dir].iter())
            .chain(args)
            .map(|arg| arg.to_string())
            .collect();
        ran(self.ask(&Ask::Run {
            args: run,
            cwd: dir.to_string(),
            limit: limit as u64,
        }))
    }

    fn stat(&self, path: &str) -> Option<Meta> {
        let path = path.to_string();
        match self.ask(&Ask::Stat { path }) {
            Answer::Stat { entry: Some(entry) } => Some(Meta {
                len: entry.len,
                modified: entry
                    .modified
                    .map(|secs| std::time::UNIX_EPOCH + Duration::from_secs(secs)),
                file: entry.kind == thinkterm_plugin_sdk::panel::EntryKind::File,
                link: entry.target,
            }),
            _ => None,
        }
    }

    fn read(&self, path: &str, limit: usize) -> Result<(Vec<u8>, bool), String> {
        let path = path.to_string();
        match self.ask(&Ask::Read {
            path,
            limit: limit as u64,
        }) {
            Answer::Read { bytes, cut } => Ok((bytes.0, cut)),
            Answer::Failed { why, .. } => Err(why),
            other => Err(format!("ThinkTerm answered something else: {other:?}")),
        }
    }

    fn far(&self) -> bool {
        true
    }
}

/// What git run through ThinkTerm came to, as git run here comes to it:
/// what it printed when it answered, and why not when it did not -- 127
/// being a shell's word for a program it cannot find.
fn ran(answer: Answer) -> Result<(Vec<u8>, bool), Failure> {
    match answer {
        Answer::Ran { out, cut: true, .. } => Ok((out.0, true)),
        Answer::Ran {
            status: Some(0),
            out,
            ..
        } => Ok((out.0, false)),
        Answer::Ran {
            status: Some(127), ..
        } => Err(Failure::NoGit),
        Answer::Ran {
            status: Some(_), ..
        } => Err(Failure::Refused),
        Answer::Ran { status: None, .. } => Err(Failure::Failed("git was stopped".into())),
        Answer::Failed { why, .. } => Err(Failure::Failed(why)),
        other => Err(Failure::Failed(format!(
            "ThinkTerm answered something else: {other:?}"
        ))),
    }
}

/// A place to look at, and the panels there: what each wants of it, and
/// their views, which what is asked of another machine goes on behalf of.
struct Job {
    views: Vec<u64>,
    wants: Vec<Want>,
}

/// Where the fetcher goes next, and whether it looks at each place afresh
/// or only reads the diffs picked there ([`State::picking`]).
struct Turn {
    jobs: BTreeMap<Dir, Job>,
    look: bool,
}

/// Looks for as long as the plugin runs, telling the panels to draw anew
/// whenever something changed.
pub fn fetch(shared: Shared, emitter: Emitter) {
    let mut counted: HashMap<Dir, Counted> = HashMap::new();
    let mut versions = 0u32;
    let mut wait = EVERY;
    loop {
        let Turn { jobs, look } = next_jobs(&shared, wait);
        // What was counted in a directory left last goes with its place.
        let left = lock(&shared).left.clone();
        counted.retain(|dir, _| jobs.contains_key(dir) || left.contains(dir));
        let started = Instant::now();
        let mut changed = false;
        for (dir, job) in jobs {
            let machine: Box<dyn Machine> = match &dir.host {
                None => Box::new(git::Here),
                Some(host) => Box::new(There {
                    emitter: emitter.clone(),
                    view: job.views[0],
                    machine: host.machine.clone(),
                }),
            };
            if !look {
                // A file picked: its diff alone, against the files the last
                // look listed. The next look lists them again.
                changed |= read_picked(&shared, &*machine, &dir, &job.wants, &mut versions);
                continue;
            }
            let found = git::look(
                &*machine,
                &dir.path,
                counted.entry(dir.clone()).or_default(),
            );
            let place = match found {
                Ok(Found::NoGit) => Place::NoGit,
                Ok(Found::NotRepo) => Place::NotRepo,
                Err(why) => Place::Failed(why),
                Ok(Found::Repo(snapshot)) => {
                    let shown = shown_files(&snapshot, &job.wants);
                    let read =
                        read_files(&shared, &*machine, &dir, &snapshot, &shown, &mut versions);
                    let diffs = match lock(&shared).places.get(&dir) {
                        Some(Place::Repo(before)) => {
                            keep_diffs(read, &before.diffs, &snapshot, &shown)
                        }
                        _ => read,
                    };
                    Place::Repo(Repo {
                        snapshot,
                        revision: 0,
                        diffs,
                    })
                }
            };
            changed |= settle(&mut lock(&shared), dir, place, &mut versions);
        }
        if look {
            // A repository git takes long over is looked at less often.
            wait = EVERY.max(started.elapsed() * 4);
        }
        if changed {
            emitter.redraw();
        }
    }
}

/// Waits for a panel to want a look or a file read, or for `wait` to pass
/// while one is on show: then what to look at, each directory a panel is in
/// with what the panels there want of it, and whether it is looked at
/// afresh. The places no panel is in are let go but for those left last;
/// once the last panel has gone, nothing is to be looked at, once, so that
/// the fetcher lets go of what it keeps for the others too, before it waits.
fn next_jobs(shared: &Shared, wait: Duration) -> Turn {
    let mut state = lock(shared);
    let mut due = false;
    loop {
        if state.views.is_empty() {
            if std::mem::take(&mut state.looking) {
                state.keep_places();
                return Turn {
                    jobs: BTreeMap::new(),
                    look: true,
                };
            }
            state = shared
                .1
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            continue;
        }
        if state.poked || state.picking {
            break;
        }
        let (next, timeout) = shared
            .1
            .wait_timeout(state, wait)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = next;
        if timeout.timed_out() && !state.views.is_empty() {
            due = true;
            break;
        }
    }
    let look = due || state.poked;
    state.poked = false;
    state.picking = false;
    state.looking = true;
    let mut jobs: BTreeMap<Dir, Job> = BTreeMap::new();
    for (view, want) in &state.views {
        if let Some(dir) = &want.dir {
            let job = jobs.entry(dir.clone()).or_insert_with(|| Job {
                views: Vec::new(),
                wants: Vec::new(),
            });
            job.views.push(*view);
            job.wants.push(want.clone());
        }
    }
    state.keep_places();
    Turn { jobs, look }
}

/// The files the panels that want `wants` show of `snapshot`, as
/// [`State::shown`] finds them: each one's pick, or its first file when it
/// shows the lines itself.
fn shown_files<'a>(snapshot: &'a Snapshot, wants: &[Want]) -> Vec<&'a Change> {
    let mut shown: Vec<&Change> = Vec::new();
    for want in wants {
        let picked = want
            .picked
            .as_ref()
            .and_then(|path| snapshot.files.iter().find(|file| &file.path == path));
        let first = want.inside.then(|| snapshot.files.first()).flatten();
        if let Some(file) = picked.or(first) {
            if !shown.iter().any(|kept| kept.path == file.path) {
                shown.push(file);
            }
        }
    }
    shown
}

/// The diffs of `files`, each read again and parsed only when git prints
/// something new for it.
fn read_files(
    shared: &Shared,
    machine: &dyn Machine,
    dir: &Dir,
    snapshot: &Snapshot,
    files: &[&Change],
    versions: &mut u32,
) -> HashMap<String, Read> {
    let mut diffs = HashMap::new();
    for &file in files {
        let raw = git::diff(machine, snapshot, file);
        let hash = {
            let mut hasher = DefaultHasher::new();
            raw.hash(&mut hasher);
            hasher.finish()
        };
        let bytes = raw.as_ref().map_or(0, |(out, _)| out.len());
        let known = match lock(shared).places.get(dir) {
            Some(Place::Repo(repo)) => repo
                .diffs
                .get(&file.path)
                .filter(|read| read.raw == hash)
                .map(|read| (Arc::clone(&read.diff), read.version)),
            _ => None,
        };
        let (diff, version) = match known {
            Some(known) => known,
            None => {
                let diff = match raw {
                    Ok((out, cut)) => git::lines(&out, cut, file.status == Status::Untracked),
                    Err(why) => git::failed(why),
                };
                *versions = versions.wrapping_add(1);
                (Arc::new(diff), *versions)
            }
        };
        diffs.insert(
            file.path.clone(),
            Read {
                diff,
                version,
                raw: hash,
                of: file.clone(),
                bytes,
                shown: Instant::now(),
            },
        );
    }
    diffs
}

/// Reads the diffs the panels in `dir` show that none is kept of -- that
/// of a file just picked -- against the files the last look there listed;
/// true when one was read. While nothing is listed there, the look that
/// lists it reads it.
fn read_picked(
    shared: &Shared,
    machine: &dyn Machine,
    dir: &Dir,
    wants: &[Want],
    versions: &mut u32,
) -> bool {
    let (snapshot, unread) = {
        let state = lock(shared);
        let Some(Place::Repo(repo)) = state.places.get(dir) else {
            return false;
        };
        let unread: Vec<Change> = shown_files(&repo.snapshot, wants)
            .into_iter()
            .filter(|file| !repo.diffs.contains_key(&file.path))
            .cloned()
            .collect();
        if unread.is_empty() {
            return false;
        }
        (repo.snapshot.clone(), unread)
    };
    let unread: Vec<&Change> = unread.iter().collect();
    let read = read_files(shared, machine, dir, &snapshot, &unread, versions);
    let mut state = lock(shared);
    match state.places.get_mut(dir) {
        // Unless it was let go, or looked at afresh, meanwhile.
        Some(Place::Repo(repo)) if repo.snapshot == snapshot => {
            let before = std::mem::take(&mut repo.diffs);
            repo.diffs = keep_diffs(read, &before, &snapshot, &shown_files(&snapshot, wants));
            true
        }
        _ => false,
    }
}

/// What a directory keeps of the diffs it has: those `read` now, the rest
/// of those `shown`, and of those shown before, each whose file `snapshot`
/// lists as it was listed when read, the last shown first, to at most
/// [`KEPT_DIFFS`] files and [`KEPT_DIFF_BYTES`].
fn keep_diffs(
    mut read: HashMap<String, Read>,
    before: &HashMap<String, Read>,
    snapshot: &Snapshot,
    shown: &[&Change],
) -> HashMap<String, Read> {
    let mut rest: Vec<(&String, &Read)> = Vec::new();
    for (path, old) in before {
        if read.contains_key(path) || !snapshot.files.contains(&old.of) {
            continue;
        }
        if shown.iter().any(|file| &file.path == path) {
            read.insert(path.clone(), old.clone());
        } else {
            rest.push((path, old));
        }
    }
    rest.sort_by_key(|(_, read)| std::cmp::Reverse(read.shown));
    let mut bytes = 0;
    for (path, old) in rest.into_iter().take(KEPT_DIFFS) {
        if bytes + old.bytes > KEPT_DIFF_BYTES {
            continue;
        }
        bytes += old.bytes;
        read.insert(path.clone(), old.clone());
    }
    read
}

/// Takes in what was found in `dir`; true when it differs from what was.
fn settle(state: &mut State, dir: Dir, mut place: Place, versions: &mut u32) -> bool {
    let before = state.places.get(&dir);
    let changed = match (before, &mut place) {
        (Some(Place::Repo(before)), Place::Repo(repo)) => {
            let same_files = before.snapshot == repo.snapshot;
            repo.revision = if same_files {
                before.revision
            } else {
                *versions = versions.wrapping_add(1);
                *versions
            };
            let same_diffs = before.diffs.len() == repo.diffs.len()
                && repo.diffs.iter().all(|(path, read)| {
                    before
                        .diffs
                        .get(path)
                        .is_some_and(|old| old.version == read.version)
                });
            !same_files || !same_diffs
        }
        (_, Place::Repo(repo)) => {
            *versions = versions.wrapping_add(1);
            repo.revision = *versions;
            true
        }
        (Some(Place::NoGit), Place::NoGit) | (Some(Place::NotRepo), Place::NotRepo) => false,
        (Some(Place::Failed(before)), Place::Failed(why)) => before != why,
        _ => true,
    };
    // A directory no panel is in any more is not kept, unless it was left
    // with what was found there.
    let wanted = state
        .views
        .values()
        .any(|want| want.dir.as_ref() == Some(&dir));
    if wanted || state.left.contains(&dir) {
        state.places.insert(dir, place);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn here(path: &str) -> Dir {
        Dir {
            host: None,
            path: path.into(),
        }
    }

    fn server_a() -> Host {
        Host {
            name: "server-a".into(),
            machine: "m1".into(),
        }
    }

    fn change(path: &str) -> Change {
        Change {
            path: path.to_string(),
            from: None,
            status: Status::Modified,
            counts: Some((1, 1)),
        }
    }

    fn snapshot(paths: &[&str]) -> Snapshot {
        Snapshot {
            root: "/home/user/project".into(),
            branch: "main".into(),
            unborn: false,
            base: "HEAD".into(),
            files: paths.iter().map(|path| change(path)).collect(),
            more: 0,
        }
    }

    fn repo(paths: &[&str], diffs: &[(&str, u32)]) -> Place {
        Place::Repo(Repo {
            snapshot: snapshot(paths),
            revision: 0,
            diffs: diffs
                .iter()
                .map(|(path, version)| {
                    let read = Read::new(change(path), FileDiff::default(), *version);
                    (path.to_string(), read)
                })
                .collect(),
        })
    }

    #[test]
    fn what_is_found_again_the_same_changes_nothing() {
        let dir = here("/home/user/project");
        let mut state = State::default();
        state.views.insert(
            1,
            Want {
                dir: Some(dir.clone()),
                ..Want::default()
            },
        );
        let mut versions = 0;
        assert!(settle(
            &mut state,
            dir.clone(),
            repo(&["a"], &[("a", 1)]),
            &mut versions
        ));
        let revision = |state: &State| match state.places.get(&dir) {
            Some(Place::Repo(repo)) => repo.revision,
            _ => panic!("no repository"),
        };
        let first = revision(&state);
        assert!(!settle(
            &mut state,
            dir.clone(),
            repo(&["a"], &[("a", 1)]),
            &mut versions
        ));
        assert_eq!(
            revision(&state),
            first,
            "the same files keep their revision"
        );
        assert!(settle(
            &mut state,
            dir.clone(),
            repo(&["a"], &[("a", 2)]),
            &mut versions
        ));
        assert_eq!(revision(&state), first, "a file's diff is not the list");
        assert!(settle(
            &mut state,
            dir.clone(),
            repo(&["a", "b"], &[("a", 2)]),
            &mut versions
        ));
        assert_ne!(revision(&state), first);
        assert!(settle(
            &mut state,
            dir.clone(),
            Place::NotRepo,
            &mut versions
        ));
        assert!(!settle(
            &mut state,
            dir.clone(),
            Place::NotRepo,
            &mut versions
        ));
    }

    #[test]
    fn a_panel_shows_the_file_it_picked_while_that_is_changed_still() {
        let Place::Repo(repo) = repo(&["a", "b"], &[]) else {
            unreachable!()
        };
        let mut state = State::default();
        let path = |file: Option<&Change>| file.map(|file| file.path.clone());
        state.views.insert(1, Want::default());
        assert_eq!(
            path(state.shown(1, &repo)),
            None,
            "none picked, for an extended view to show"
        );
        state.views.get_mut(&1).unwrap().inside = true;
        assert_eq!(
            path(state.shown(1, &repo)).as_deref(),
            Some("a"),
            "the first, in the panel itself"
        );
        state.views.get_mut(&1).unwrap().picked = Some("b".into());
        assert_eq!(path(state.picked(1, &repo)).as_deref(), Some("b"));
        assert_eq!(path(state.shown(1, &repo)).as_deref(), Some("b"));
        state.views.get_mut(&1).unwrap().picked = Some("committed".into());
        assert_eq!(path(state.picked(1, &repo)), None);
        assert_eq!(path(state.shown(1, &repo)).as_deref(), Some("a"));
    }

    #[test]
    fn a_place_no_panel_is_in_is_not_kept() {
        let mut state = State::default();
        let mut versions = 0;
        settle(
            &mut state,
            here("/home/user/gone"),
            Place::NotRepo,
            &mut versions,
        );
        assert!(state.places.is_empty());
    }

    #[test]
    fn what_was_found_goes_with_the_panels_it_was_for() {
        let shared = Shared::default();
        let (here, there) = (
            here("/home/user/project"),
            Dir {
                host: Some(server_a()),
                path: "/home/user/project".into(),
            },
        );
        place(&shared, 1, Some(here.clone()), false);
        place(&shared, 2, Some(there.clone()), false);
        let jobs = next_jobs(&shared, Duration::ZERO).jobs;
        assert_eq!(jobs.len(), 2, "the same path on two machines is two places");
        assert_eq!(jobs[&there].views, [2], "asked on the panel's behalf");
        {
            let mut state = lock(&shared);
            state.places.insert(here, Place::NotRepo);
            state.places.insert(there, Place::NotRepo);
        }
        close(&shared, 2);
        assert_eq!(lock(&shared).places.len(), 2, "kept, left last");
        assert_eq!(
            next_jobs(&shared, Duration::ZERO).jobs.len(),
            1,
            "and not looked at"
        );
        close(&shared, 1);
        assert!(
            next_jobs(&shared, Duration::ZERO).jobs.is_empty(),
            "nothing is looked at once the last panel went"
        );
        let state = lock(&shared);
        assert_eq!(state.places.len(), 2, "what was found is kept, though");
        assert_eq!(state.left.len(), 2);
    }

    #[test]
    fn a_panel_shown_again_shows_what_it_showed_with_the_file_it_picked() {
        let shared = Shared::default();
        let repo = here("/home/user/project");
        place(&shared, 1, Some(repo.clone()), false);
        lock(&shared).places.insert(repo.clone(), Place::NotRepo);
        pick(&shared, 1, Some("src/main.rs".into()));
        // Looked at while on show.
        assert_eq!(next_jobs(&shared, Duration::ZERO).jobs.len(), 1);
        // The sidebar goes to another of its panels, or closes: the view
        // goes, and the fetcher is let go.
        close(&shared, 1);
        assert!(next_jobs(&shared, Duration::ZERO).jobs.is_empty());
        // Shown again, as a new view.
        place(&shared, 2, Some(repo.clone()), false);
        let state = lock(&shared);
        assert!(state.places.contains_key(&repo), "at once");
        assert_eq!(
            state.views[&2].picked.as_deref(),
            Some("src/main.rs"),
            "with the file it picked"
        );
        drop(state);
        // Put down, it is not picked again.
        pick(&shared, 2, None);
        place(&shared, 2, Some(here("/home/user/other")), false);
        place(&shared, 2, Some(repo), false);
        assert_eq!(lock(&shared).views[&2].picked, None);
    }

    #[test]
    fn a_panel_back_in_a_directory_it_left_shows_what_was_found_there() {
        let shared = Shared::default();
        let found = |state: &State, dir: &Dir| state.places.contains_key(dir);
        // A tab in the repository, another in a directory of it: the
        // panel follows the terminal from one to the other.
        let (root, src) = (here("/home/user/project"), here("/home/user/project/src"));
        place(&shared, 1, Some(root.clone()), false);
        lock(&shared).places.insert(root.clone(), Place::NotRepo);
        place(&shared, 1, Some(src.clone()), false);
        next_jobs(&shared, Duration::ZERO);
        assert!(found(&lock(&shared), &root), "kept while the panel is away");
        lock(&shared).places.insert(src.clone(), Place::NotRepo);
        place(&shared, 1, Some(root.clone()), false);
        let jobs = next_jobs(&shared, Duration::ZERO).jobs;
        let state = lock(&shared);
        assert!(found(&state, &root), "there at once, and looked at again");
        assert!(jobs.contains_key(&root) && !jobs.contains_key(&src));
        assert!(found(&state, &src), "the one left is kept in turn");
        drop(state);

        // No more than a few are kept: the one left first goes.
        for n in 0..KEPT + 1 {
            let dir = here(&format!("/home/user/other{n}"));
            place(&shared, 1, Some(dir.clone()), false);
            lock(&shared).places.insert(dir, Place::NotRepo);
        }
        next_jobs(&shared, Duration::ZERO);
        let state = lock(&shared);
        assert_eq!(state.left.len(), KEPT);
        assert!(!found(&state, &root) && !found(&state, &src), "left first");
        assert_eq!(
            state.places.len(),
            KEPT + 1,
            "those kept, and the one in use"
        );
    }

    #[test]
    fn git_run_elsewhere_comes_to_what_git_run_here_does() {
        use thinkterm_plugin_sdk::panel::Bytes;
        let run = |status, cut| Answer::Ran {
            status,
            out: Bytes(b"out".to_vec()),
            cut,
        };
        assert_eq!(ran(run(Some(0), false)), Ok((b"out".to_vec(), false)));
        assert_eq!(
            ran(run(None, true)),
            Ok((b"out".to_vec(), true)),
            "cut short"
        );
        assert_eq!(ran(run(Some(127), false)), Err(Failure::NoGit));
        assert_eq!(ran(run(Some(128), false)), Err(Failure::Refused));
        assert!(matches!(ran(run(None, false)), Err(Failure::Failed(_))));
        assert_eq!(
            ran(Answer::failed("not connected")),
            Err(Failure::Failed("not connected".into()))
        );
    }

    #[test]
    fn a_terminal_here_or_there_is_a_directory_named_so() {
        let dir = Dir {
            host: Some(server_a()),
            path: "/srv/app".into(),
        };
        assert_eq!(dir.to_string(), "server-a:/srv/app");
        assert_eq!(here("/srv/app").to_string(), "/srv/app");
        let other_account = Dir {
            host: Some(Host {
                machine: "m2".into(),
                ..server_a()
            }),
            path: "/srv/app".into(),
        };
        assert_ne!(dir, other_account, "another account on the host");
        assert_ne!(dir.key(), other_account.key());
        assert_eq!(dir.to_string(), other_account.to_string(), "shown alike");
    }
    /// A machine that runs no git, and tells what it was asked to run.
    #[derive(Default)]
    struct Asked(std::cell::RefCell<Vec<String>>);

    impl Machine for Asked {
        fn git(
            &self,
            _dir: &str,
            args: &[&str],
            _limit: usize,
        ) -> Result<(Vec<u8>, bool), Failure> {
            self.0.borrow_mut().push(args.join(" "));
            let path = args.last().copied().unwrap_or_default();
            Ok((
                format!("@@ -1 +1 @@\n-old {path}\n+new {path}\n").into_bytes(),
                false,
            ))
        }

        fn stat(&self, _path: &str) -> Option<Meta> {
            None
        }

        fn read(&self, _path: &str, _limit: usize) -> Result<(Vec<u8>, bool), String> {
            Ok((Vec::new(), false))
        }
    }

    fn diffs_in(shared: &Shared, dir: &Dir) -> Vec<String> {
        match lock(shared).places.get(dir) {
            Some(Place::Repo(repo)) => {
                let mut paths: Vec<String> = repo.diffs.keys().cloned().collect();
                paths.sort();
                paths
            }
            _ => panic!("no repository"),
        }
    }

    #[test]
    fn a_pick_asks_for_no_look_and_a_panel_moving_or_the_time_passing_does() {
        let shared = Shared::default();
        let dir = here("/home/user/project");
        place(&shared, 1, Some(dir.clone()), false);
        assert!(next_jobs(&shared, Duration::ZERO).look, "a panel came");
        pick(&shared, 1, Some("a".into()));
        let turn = next_jobs(&shared, Duration::from_secs(60));
        assert!(!turn.look, "at once, and the picked file's diff alone");
        assert_eq!(turn.jobs[&dir].wants[0].picked.as_deref(), Some("a"));
        assert!(next_jobs(&shared, Duration::ZERO).look, "in time, a look");
    }

    #[test]
    fn a_file_picked_is_read_alone_and_one_read_before_is_not_read_again() {
        let shared = Shared::default();
        let dir = here("/home/user/project");
        place(&shared, 1, Some(dir.clone()), false);
        lock(&shared)
            .places
            .insert(dir.clone(), repo(&["a", "b"], &[]));
        // The look the panel's coming asked for, taken.
        assert!(next_jobs(&shared, Duration::ZERO).look);
        let machine = Asked::default();
        let mut versions = 0;
        let mut read_picked_now = |path: &str| {
            pick(&shared, 1, Some(path.into()));
            let turn = next_jobs(&shared, Duration::from_secs(60));
            assert!(!turn.look);
            read_picked(
                &shared,
                &machine,
                &dir,
                &turn.jobs[&dir].wants,
                &mut versions,
            )
        };
        assert!(read_picked_now("a"));
        assert!(read_picked_now("b"));
        assert!(!read_picked_now("a"), "kept from before: nothing to read");
        let asked = machine.0.take();
        assert_eq!(asked.len(), 2, "one git a file, and no look: {asked:?}");
        assert!(asked[0].starts_with("diff HEAD") && asked[0].ends_with("-- a"));
        assert!(asked[1].ends_with("-- b"));
        assert_eq!(diffs_in(&shared, &dir), ["a", "b"], "the one put down kept");
    }

    #[test]
    fn a_diff_is_kept_while_its_file_is_listed_as_it_was_and_no_more_of_them_than_so_many() {
        let read = |path: &str, bytes: usize, ago: u64| {
            let mut read = Read::new(change(path), FileDiff::default(), 1);
            read.bytes = bytes;
            read.shown = Instant::now() - Duration::from_secs(ago);
            (path.to_string(), read)
        };
        let before: HashMap<String, Read> =
            [read("a", 10, 3), read("b", 10, 2), read("c", 10, 1)].into();
        let mut listed = snapshot(&["a", "b"]);
        listed.files[0].counts = Some((2, 1));
        let shown = [&listed.files[1]];
        let kept = keep_diffs(HashMap::new(), &before, &listed, &shown);
        let mut paths: Vec<&String> = kept.keys().collect();
        paths.sort();
        assert_eq!(paths, ["b"], "a changed since, c committed");

        // Of those no panel shows, the last shown, up to so many and so much.
        let many: Vec<String> = (0..KEPT_DIFFS + 3).map(|n| format!("f{n}")).collect();
        let names: Vec<&str> = many.iter().map(String::as_str).collect();
        let listed = snapshot(&names);
        let before: HashMap<String, Read> = many
            .iter()
            .enumerate()
            .map(|(n, path)| read(path, 10, n as u64))
            .collect();
        let kept = keep_diffs(HashMap::new(), &before, &listed, &[]);
        assert_eq!(kept.len(), KEPT_DIFFS);
        assert!(kept.contains_key("f0") && !kept.contains_key(&format!("f{}", KEPT_DIFFS)));
        let before: HashMap<String, Read> =
            [read("big", KEPT_DIFF_BYTES + 1, 0), read("small", 10, 5)].into();
        let listed = snapshot(&["big", "small"]);
        let kept = keep_diffs(HashMap::new(), &before, &listed, &[]);
        assert!(!kept.contains_key("big") && kept.contains_key("small"));
        let shown = [&listed.files[0]];
        let kept = keep_diffs(HashMap::new(), &before, &listed, &shown);
        assert!(
            kept.contains_key("big"),
            "shown, it is kept whatever it costs"
        );
    }
}
