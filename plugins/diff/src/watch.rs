//! The fetcher: git asked, on a thread of its own, about the directories the
//! panels on show are in -- every [`EVERY`], or less often in a repository
//! git takes a while over, and at once when a panel comes, moves or picks a
//! file -- and the panels drawn again when what it says changed. Nothing is
//! asked while no panel is on show. A directory on another machine is
//! looked at there, through ThinkTerm ([`There`]).

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
    /// The picked files' diffs, by path, as last read.
    pub diffs: HashMap<String, Read>,
}

pub struct Read {
    pub diff: Arc<FileDiff>,
    /// Changes whenever the diff does.
    pub version: u32,
    /// What git printed, hashed: the same again is the same diff.
    raw: u64,
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
    pub fn new(diff: FileDiff, version: u32) -> Self {
        Self {
            diff: Arc::new(diff),
            version,
            raw: 0,
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

/// Panel `panel` picked the file at `path`, or put it down: read at once,
/// and picked again when a panel comes back to the directory.
pub fn pick(shared: &Shared, panel: u64, path: Option<String>) {
    let mut state = lock(shared);
    let State {
        views,
        picked_in,
        poked,
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
        *poked = true;
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

/// Looks for as long as the plugin runs, telling the panels to draw anew
/// whenever something changed.
pub fn fetch(shared: Shared, emitter: Emitter) {
    let mut counted: HashMap<Dir, Counted> = HashMap::new();
    let mut versions = 0u32;
    let mut wait = EVERY;
    loop {
        let jobs = next_jobs(&shared, wait);
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
                    let diffs = read_diffs(
                        &shared,
                        &*machine,
                        &dir,
                        &snapshot,
                        &job.wants,
                        &mut versions,
                    );
                    Place::Repo(Repo {
                        snapshot,
                        revision: 0,
                        diffs,
                    })
                }
            };
            changed |= settle(&mut lock(&shared), dir, place, &mut versions);
        }
        // A repository git takes long over is looked at less often.
        wait = EVERY.max(started.elapsed() * 4);
        if changed {
            emitter.redraw();
        }
    }
}

/// Waits for a panel to want a look, or for `wait` to pass while one is on
/// show: then what to look at, each directory a panel is in with what the
/// panels there want of it. The places no panel is in are let go but for
/// those left last; once the last panel has gone, nothing is to be looked
/// at, once, so that the fetcher lets go of what it keeps for the others
/// too, before it waits.
fn next_jobs(shared: &Shared, wait: Duration) -> BTreeMap<Dir, Job> {
    let mut state = lock(shared);
    loop {
        if state.views.is_empty() {
            if std::mem::take(&mut state.looking) {
                state.keep_places();
                return BTreeMap::new();
            }
            state = shared
                .1
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            continue;
        }
        if state.poked {
            break;
        }
        let (next, timeout) = shared
            .1
            .wait_timeout(state, wait)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state = next;
        if timeout.timed_out() && !state.views.is_empty() {
            break;
        }
    }
    state.poked = false;
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
    jobs
}

/// The diffs of the files the panels in `dir` show, as [`State::shown`]
/// finds them, each read again and parsed only when git prints something
/// new for it.
fn read_diffs(
    shared: &Shared,
    machine: &dyn Machine,
    dir: &Dir,
    snapshot: &Snapshot,
    wants: &[Want],
    versions: &mut u32,
) -> HashMap<String, Read> {
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
    let mut diffs = HashMap::new();
    for file in shown {
        let raw = git::diff(machine, snapshot, file);
        let hash = {
            let mut hasher = DefaultHasher::new();
            raw.hash(&mut hasher);
            hasher.finish()
        };
        let known = match lock(shared).places.get(dir) {
            Some(Place::Repo(repo)) => repo
                .diffs
                .get(&file.path)
                .filter(|read| read.raw == hash)
                .map(|read| (Arc::clone(&read.diff), read.version)),
            _ => None,
        };
        let read = match known {
            Some((diff, version)) => Read {
                diff,
                version,
                raw: hash,
            },
            None => {
                let diff = match raw {
                    Ok((bytes, cut)) => git::lines(&bytes, cut, file.status == Status::Untracked),
                    Err(why) => git::failed(why),
                };
                *versions = versions.wrapping_add(1);
                Read {
                    diff: Arc::new(diff),
                    version: *versions,
                    raw: hash,
                }
            }
        };
        diffs.insert(file.path.clone(), read);
    }
    diffs
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

    fn snapshot(paths: &[&str]) -> Snapshot {
        Snapshot {
            root: "/home/user/project".into(),
            branch: "main".into(),
            unborn: false,
            base: "HEAD".into(),
            files: paths
                .iter()
                .map(|path| Change {
                    path: path.to_string(),
                    from: None,
                    status: Status::Modified,
                    counts: Some((1, 1)),
                })
                .collect(),
            more: 0,
        }
    }

    fn repo(paths: &[&str], diffs: &[(&str, u32)]) -> Place {
        Place::Repo(Repo {
            snapshot: snapshot(paths),
            revision: 0,
            diffs: diffs
                .iter()
                .map(|(path, version)| (path.to_string(), Read::new(FileDiff::default(), *version)))
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
        let jobs = next_jobs(&shared, Duration::ZERO);
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
            next_jobs(&shared, Duration::ZERO).len(),
            1,
            "and not looked at"
        );
        close(&shared, 1);
        assert!(
            next_jobs(&shared, Duration::ZERO).is_empty(),
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
        assert_eq!(next_jobs(&shared, Duration::ZERO).len(), 1);
        // The sidebar goes to another of its panels, or closes: the view
        // goes, and the fetcher is let go.
        close(&shared, 1);
        assert!(next_jobs(&shared, Duration::ZERO).is_empty());
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
        let jobs = next_jobs(&shared, Duration::ZERO);
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
}
