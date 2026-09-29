//! What git says about a directory: the repository it is in, the files that
//! changed there since the last commit, and how each one changed. Read from
//! git's own output. The programs a repository's configuration can name --
//! an fsmonitor, an external diff, a text converter -- are not run for it.
//!
//! Git runs, and the files are read, on the machine the directory is on
//! ([`Machine`]): this one, or another that ThinkTerm reaches for the
//! plugin. Paths are git's, with `/` between names on every system.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};
use unicode_width::UnicodeWidthChar;

/// How long git may take to answer before it is given up on.
const PATIENCE: Duration = Duration::from_secs(20);
/// The most of a list git prints that is read.
const LIST_BYTES: usize = 16 * 1024 * 1024;
/// The most of one file's diff read: a file changed beyond it is shown in
/// part.
pub const DIFF_BYTES: usize = 8 * 1024 * 1024;
/// The most lines of one file's diff kept.
pub const DIFF_LINES: usize = 50_000;
/// The most files listed.
pub const FILES: usize = 2_000;
/// The longest line kept, in characters: what a panel draws of one.
const LINE_CHARS: usize = 2_000;
/// An untracked file bigger than this is not read to count its lines.
const COUNT_BYTES: u64 = 256 * 1024;
/// The most untracked files read in one look to count their lines.
const COUNT_READS: usize = 200;
/// Columns from one tab stop to the next.
const TAB: usize = 4;
/// What git is run with before what is asked of it: no lock taken, a path a
/// name and never a pattern, and none of the programs or colours a
/// repository's configuration can name.
pub const GIT_OPTIONS: [&str; 8] = [
    "--no-optional-locks",
    "--literal-pathspecs",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.quotepath=false",
    "-c",
    "color.ui=false",
];

/// Where git runs and the files are read: this machine, or another.
pub trait Machine {
    /// Runs git in `dir` with `args`, keeping at most `limit` bytes of what
    /// it prints: that, and whether there was more.
    fn git(&self, dir: &str, args: &[&str], limit: usize) -> Result<(Vec<u8>, bool), Failure>;
    /// What is at `path`, a link not followed: `None` when nothing is, or it
    /// cannot be seen.
    fn stat(&self, path: &str) -> Option<Meta>;
    /// At most `limit` bytes of the file at `path`, and whether it has more.
    fn read(&self, path: &str, limit: usize) -> Result<(Vec<u8>, bool), String>;
    /// Whether each of those is a trip to another machine: then the lines
    /// of new files are not counted, which would cost one a file every look.
    fn far(&self) -> bool {
        false
    }
}

/// A file, as [`Machine::stat`] finds it.
#[derive(Debug, Clone, PartialEq)]
pub struct Meta {
    pub len: u64,
    /// When it last changed, as finely as the machine says: a change
    /// within the second, to a file of the same size, is one too.
    pub modified: Option<SystemTime>,
    pub file: bool,
    /// Where a link points.
    pub link: Option<String>,
}

/// This machine: git run here, and the files read from its disks.
pub struct Here;

impl Machine for Here {
    fn git(&self, dir: &str, args: &[&str], limit: usize) -> Result<(Vec<u8>, bool), Failure> {
        git(Path::new(dir), args, limit)
    }

    fn stat(&self, path: &str) -> Option<Meta> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        let link = meta
            .file_type()
            .is_symlink()
            .then(|| std::fs::read_link(path).ok())
            .flatten()
            .map(|target| target.to_string_lossy().into_owned());
        Some(Meta {
            len: meta.len(),
            modified: meta.modified().ok(),
            file: meta.is_file(),
            link,
        })
    }

    fn read(&self, path: &str, limit: usize) -> Result<(Vec<u8>, bool), String> {
        let failed = |err: io::Error| format!("{path}: {err}");
        let file = std::fs::File::open(path).map_err(failed)?;
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(failed)?;
        let cut = bytes.len() > limit;
        bytes.truncate(limit);
        Ok((bytes, cut))
    }
}

/// `path`, from the repository's root `root`.
pub fn joined(root: &str, path: &str) -> String {
    format!("{}/{path}", root.trim_end_matches('/'))
}

/// Why git gave no answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// There is no git to ask.
    NoGit,
    /// It ran, and said no.
    Refused,
    /// It could not be run, or took too long.
    Failed(String),
}

/// A directory, as git sees it.
#[derive(Debug, Clone, PartialEq)]
pub enum Found {
    NoGit,
    /// It is in no repository.
    NotRepo,
    Repo(Snapshot),
}

/// A repository's changes since its last commit, the working tree's and
/// the index's alike.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// As git prints it.
    pub root: String,
    /// The branch checked out, or the short name of the commit when none is.
    pub branch: String,
    /// The branch has no commit yet: everything in it is new.
    pub unborn: bool,
    /// What the changes are counted from: `HEAD`, or the empty tree.
    pub base: String,
    pub files: Vec<Change>,
    /// Files changed beyond those listed.
    pub more: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// From the repository's root, `/` between names.
    pub path: String,
    /// Where a renamed or copied file was.
    pub from: Option<String>,
    pub status: Status,
    /// Lines gained and lost; `None` for a binary file, or one not counted.
    pub counts: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    /// New, and not yet added to the index.
    Untracked,
    /// Left in conflict by a merge.
    Conflicted,
}

/// One file's changes, a line of the panel each.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FileDiff {
    pub lines: Vec<Line>,
    /// The widest line, in columns.
    pub columns: usize,
    /// The biggest line number.
    pub biggest: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub kind: Kind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    /// With its tabs spread to their stops.
    pub text: String,
    /// The part of an edited line that changed, as a range of characters.
    pub changed: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Hunk,
    Context,
    Added,
    Removed,
    Note(Note),
}

/// What a line says about the file instead of showing one of its lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    Binary,
    /// The line above ends the file without a newline.
    NoNewline,
    /// Only the file's mode changed.
    Mode,
    /// Renamed or copied, its content the same.
    Unchanged,
    /// The rest is too long to show.
    Cut,
    /// It could not be read; the line's text says why.
    Failed,
}

/// What `dir` is in, on `machine`: a repository, and its changes. `counted`
/// keeps the lines of untracked files from one look to the next, so a file
/// is read again only when it changed; it is left with the files listed
/// now.
pub fn look(machine: &dyn Machine, dir: &str, counted: &mut Counted) -> Result<Found, String> {
    let git = |dir: &str, args: &[&str], limit| machine.git(dir, args, limit);
    let root = match git(dir, &["rev-parse", "--show-toplevel"], 64 * 1024) {
        Ok((out, _)) => line_of(&out),
        Err(Failure::NoGit) => return Ok(Found::NoGit),
        Err(Failure::Refused) => return Ok(Found::NotRepo),
        Err(Failure::Failed(why)) => return Err(why),
    };
    let unborn = git(&root, &["rev-parse", "--verify", "-q", "HEAD"], 1024).is_err();
    let base = if unborn {
        // Everything is new: counted from nothing.
        let (out, _) = git(&root, &["hash-object", "-t", "tree", "--stdin"], 1024)
            .map_err(|failure| failure.why())?;
        line_of(&out)
    } else {
        "HEAD".to_string()
    };
    let branch = match git(&root, &["symbolic-ref", "--short", "-q", "HEAD"], 1024) {
        Ok((out, _)) => line_of(&out),
        Err(_) => git(&root, &["rev-parse", "--short", "HEAD"], 1024)
            .map(|(out, _)| line_of(&out))
            .unwrap_or_default(),
    };
    let diff = |what: &str| {
        let args = [
            "diff",
            base.as_str(),
            what,
            "-z",
            "-M",
            "--no-ext-diff",
            "--no-textconv",
        ];
        git(&root, &args, LIST_BYTES).map_err(|failure| failure.why())
    };
    let (statuses, _) = diff("--name-status")?;
    let (counts, _) = diff("--numstat")?;
    let (unmerged, _) = git(&root, &["ls-files", "--unmerged", "-z"], LIST_BYTES)
        .map_err(|failure| failure.why())?;
    let others = ["ls-files", "--others", "--exclude-standard", "-z"];
    let (untracked, _) = git(&root, &others, LIST_BYTES).map_err(|failure| failure.why())?;

    let mut files = changes(&statuses, &counts, &unmerged);
    let listed: Vec<String> = split_z(&untracked)
        .filter(|path| !path.is_empty())
        .map(|path| path.into_owned())
        .collect();
    counted.keep_only(&root, &listed);
    // A first look at many new files reads a few of them at a time.
    let mut reads = if machine.far() { 0 } else { COUNT_READS };
    for path in listed {
        let counts = counted
            .lines(machine, &joined(&root, &path), &mut reads)
            .map(|lines| (lines, 0));
        files.push(Change {
            path,
            from: None,
            status: Status::Untracked,
            counts,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let more = files.len().saturating_sub(FILES);
    files.truncate(FILES);
    Ok(Found::Repo(Snapshot {
        root,
        branch,
        unborn,
        base,
        files,
        more,
    }))
}

/// How `change` changed, with its raw form: the same raw form is the same
/// diff, so a look that finds it again need not read it anew.
pub fn diff(
    machine: &dyn Machine,
    snapshot: &Snapshot,
    change: &Change,
) -> Result<(Vec<u8>, bool), String> {
    if change.status == Status::Untracked {
        return read_new(machine, &joined(&snapshot.root, &change.path));
    }
    let mut args = vec![
        "diff",
        snapshot.base.as_str(),
        "-M",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "-U3",
        "--",
        change.path.as_str(),
    ];
    // A rename is found only with both of its names in the pathspec.
    if let Some(from) = &change.from {
        args.push(from);
    }
    machine
        .git(&snapshot.root, &args, DIFF_BYTES)
        .map_err(|failure| failure.why())
}

/// The lines of the diff git printed, `cut` when it was longer than was
/// read; for an untracked file, its content, every line of it new.
pub fn lines(raw: &[u8], cut: bool, untracked: bool) -> FileDiff {
    let mut diff = FileDiff::default();
    if untracked {
        read_whole(&mut diff, raw);
    } else {
        read_unified(&mut diff, raw);
    }
    if cut || diff.lines.len() > DIFF_LINES {
        diff.lines.truncate(DIFF_LINES);
        diff.lines.push(note(Note::Cut));
    }
    mark_changes(&mut diff.lines);
    diff.columns = diff
        .lines
        .iter()
        .filter(|line| !matches!(line.kind, Kind::Note(_)))
        .map(|line| columns(&line.text))
        .max()
        .unwrap_or(0);
    diff.biggest = diff
        .lines
        .iter()
        .filter_map(|line| line.old.max(line.new))
        .max()
        .unwrap_or(0);
    diff
}

/// Lines of untracked files, as last counted: read again only when a
/// file's size or time changed.
#[derive(Default)]
pub struct Counted {
    files: std::collections::HashMap<String, (u64, Option<SystemTime>, Option<u32>)>,
}

impl Counted {
    /// Lines in the file at `path`: none for one too big to read, or
    /// binary, or when `reads` has run out and it is not counted yet.
    fn lines(&mut self, machine: &dyn Machine, path: &str, reads: &mut usize) -> Option<u32> {
        if *reads == 0 && !self.files.contains_key(path) {
            return None;
        }
        let meta = machine.stat(path)?;
        // A link is where it points, as git keeps it: one line.
        if meta.link.is_some() {
            return Some(1);
        }
        let stamp = (meta.len, meta.modified);
        if let Some((len, time, lines)) = self.files.get(path) {
            if (*len, *time) == stamp {
                return *lines;
            }
        }
        if *reads == 0 {
            return None;
        }
        *reads -= 1;
        let lines = (meta.file && meta.len <= COUNT_BYTES)
            .then(|| machine.read(path, COUNT_BYTES as usize).ok())
            .flatten()
            .map(|(bytes, _)| bytes)
            .filter(|bytes| !binary(bytes))
            .map(|bytes| count_lines(&bytes));
        self.files
            .insert(path.to_string(), (stamp.0, stamp.1, lines));
        lines
    }

    /// Lets go of every file but those in `paths`, from `root`.
    fn keep_only(&mut self, root: &str, paths: &[String]) {
        let kept: std::collections::HashSet<String> =
            paths.iter().map(|path| joined(root, path)).collect();
        self.files.retain(|path, _| kept.contains(path));
    }
}

/// Runs git in `dir` with `args`, keeping at most `limit` bytes of what it
/// prints: that, and whether there was more. Given up on, and ended, when
/// it takes longer than [`PATIENCE`].
fn git(dir: &Path, args: &[&str], limit: usize) -> Result<(Vec<u8>, bool), Failure> {
    let mut command = Command::new("git");
    command
        // A path given is a name: `*` or a leading `:` in one is no pattern.
        .args(GIT_OPTIONS)
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window flashing up for each look.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => Failure::NoGit,
        _ => Failure::Failed(format!("git: {err}")),
    })?;
    let mut stdout = child.stdout.take().expect("piped above");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let read = (&mut stdout).take(limit as u64 + 1).read_to_end(&mut out);
        let _ = tx.send(read.map(|_| out));
    });
    let out = match rx.recv_timeout(PATIENCE) {
        Ok(Ok(out)) => out,
        Ok(Err(err)) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Failure::Failed(format!("git: {err}")));
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Failure::Failed("git took too long".into()));
        }
    };
    if out.len() > limit {
        // What it had left to say is not wanted.
        let _ = child.kill();
        let _ = child.wait();
        let mut out = out;
        out.truncate(limit);
        return Ok((out, true));
    }
    let status = child
        .wait()
        .map_err(|err| Failure::Failed(format!("git: {err}")))?;
    if status.success() {
        Ok((out, false))
    } else {
        Err(Failure::Refused)
    }
}

impl Failure {
    pub fn why(self) -> String {
        match self {
            Failure::NoGit => "git is not installed".into(),
            Failure::Refused => "git refused".into(),
            Failure::Failed(why) => why,
        }
    }
}

/// The first line of `out`, without its end.
fn line_of(out: &[u8]) -> String {
    String::from_utf8_lossy(out)
        .lines()
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

/// The fields of git's `-z` output. A name that is not UTF-8 is still a
/// field, its odd bytes shown as U+FFFD: dropped, it would pair every name
/// after it with the wrong status.
fn split_z(out: &[u8]) -> impl Iterator<Item = std::borrow::Cow<'_, str>> {
    out.split(|byte| *byte == 0).map(String::from_utf8_lossy)
}

/// The files `--name-status -z` and `--numstat -z` list, with their counts,
/// and the index's conflicts, even when their content matches HEAD.
fn changes(statuses: &[u8], counts: &[u8], unmerged: &[u8]) -> Vec<Change> {
    let mut files = Vec::new();
    let mut fields = split_z(statuses).filter(|field| !field.is_empty());
    while let Some(letter) = fields.next() {
        let status = match letter.as_bytes().first() {
            Some(b'A') => Status::Added,
            Some(b'D') => Status::Deleted,
            Some(b'R') => Status::Renamed,
            Some(b'C') => Status::Copied,
            Some(b'U') => Status::Conflicted,
            _ => Status::Modified,
        };
        let (from, path) = if matches!(status, Status::Renamed | Status::Copied) {
            let from = fields.next().unwrap_or_default();
            (Some(from.into_owned()), fields.next().unwrap_or_default())
        } else {
            (None, fields.next().unwrap_or_default())
        };
        if path.is_empty() {
            break;
        }
        files.push(Change {
            path: path.into_owned(),
            from,
            status,
            counts: None,
        });
    }
    // "added\tremoved\tpath", or "added\tremoved\t" then the two names of a
    // rename as fields of their own; "-" for a binary file's counts.
    let mut by_path: HashMap<_, _> = files
        .iter_mut()
        .map(|file| (file.path.as_str(), &mut file.counts))
        .collect();
    let mut fields = split_z(counts);
    while let Some(field) = fields.next() {
        let mut parts = field.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            let _from = fields.next();
            fields.next().unwrap_or_default()
        } else {
            std::borrow::Cow::Borrowed(path)
        };
        let counts = added.parse().ok().zip(removed.parse().ok());
        if let Some(found) = by_path.get_mut(path.as_ref()) {
            **found = counts;
        }
    }
    drop(by_path);
    // ls-files emits one record per index stage, with the path after the
    // first tab. A path can itself contain tabs or newlines.
    let mut conflicts: HashSet<String> = split_z(unmerged)
        .filter_map(|field| field.split_once('\t').map(|(_, path)| path.to_string()))
        .collect();
    for file in &mut files {
        if conflicts.remove(&file.path) {
            file.status = Status::Conflicted;
        }
    }
    files.extend(conflicts.into_iter().map(|path| Change {
        path,
        from: None,
        status: Status::Conflicted,
        counts: None,
    }));
    files
}

/// An untracked file's content, as much as is read of it: for a link, where
/// it points, as git would keep it, not what it points to.
fn read_new(machine: &dyn Machine, path: &str) -> Result<(Vec<u8>, bool), String> {
    let meta = machine
        .stat(path)
        .ok_or_else(|| format!("{path}: it is not there"))?;
    if let Some(target) = meta.link {
        return Ok((target.into_bytes(), false));
    }
    machine.read(path, DIFF_BYTES)
}

/// A new file's lines, all of them added.
fn read_whole(diff: &mut FileDiff, raw: &[u8]) {
    if binary(raw) {
        diff.lines.push(note(Note::Binary));
        return;
    }
    let text = String::from_utf8_lossy(raw);
    let count = count_lines(raw);
    if count == 0 {
        return;
    }
    diff.lines.push(hunk(format!("@@ -0,0 +1,{count} @@")));
    for (index, text) in text.lines().take(DIFF_LINES).enumerate() {
        diff.lines.push(Line {
            kind: Kind::Added,
            old: None,
            new: Some(index as u32 + 1),
            text: shown(text),
            changed: None,
        });
    }
}

/// The lines of a unified diff: its hunks, their lines, and what the
/// headers say when there is no hunk.
fn read_unified(diff: &mut FileDiff, raw: &[u8]) {
    let text = String::from_utf8_lossy(raw);
    let (mut old, mut new) = (0u32, 0u32);
    let (mut hunks, mut binary, mut mode, mut header) = (false, false, false, true);
    for text in text.lines() {
        if diff.lines.len() > DIFF_LINES {
            break;
        }
        if let Some(rest) = text.strip_prefix("@@ ") {
            header = false;
            hunks = true;
            let (a, b) = hunk_starts(rest);
            (old, new) = (a, b);
            diff.lines.push(hunk(shown(text)));
            continue;
        }
        if header || text.starts_with("diff --git ") {
            header = true;
            binary |= text.starts_with("Binary files ") || text == "GIT binary patch";
            mode |= text.starts_with("old mode ") || text.starts_with("new mode ");
            continue;
        }
        let (kind, body) = match text.as_bytes().first() {
            Some(b'+') => (Kind::Added, &text[1..]),
            Some(b'-') => (Kind::Removed, &text[1..]),
            Some(b' ') => (Kind::Context, &text[1..]),
            Some(b'\\') => {
                diff.lines.push(note(Note::NoNewline));
                continue;
            }
            // An empty context line, where the diff dropped the space.
            None => (Kind::Context, ""),
            Some(_) => continue,
        };
        let (at_old, at_new) = match kind {
            Kind::Added => {
                new += 1;
                (None, Some(new - 1))
            }
            Kind::Removed => {
                old += 1;
                (Some(old - 1), None)
            }
            _ => {
                old += 1;
                new += 1;
                (Some(old - 1), Some(new - 1))
            }
        };
        diff.lines.push(Line {
            kind,
            old: at_old,
            new: at_new,
            text: shown(body),
            changed: None,
        });
    }
    if !hunks {
        let said = if binary {
            Note::Binary
        } else if mode {
            Note::Mode
        } else {
            Note::Unchanged
        };
        diff.lines.push(note(said));
    }
}

/// Where a hunk starts in the old file and in the new: `-12,5 +12,7 @@`.
fn hunk_starts(rest: &str) -> (u32, u32) {
    let start = |mark: char| {
        rest.split(' ')
            .find_map(|part| part.strip_prefix(mark))
            .and_then(|range| range.split(',').next())
            .and_then(|line| line.parse::<u32>().ok())
            .unwrap_or(0)
    };
    (start('-'), start('+'))
}

/// Marks what differs between each removed line and the added line that
/// replaced it: what lies between their common start and their common
/// end, when they have enough in common for that to read as an edit.
fn mark_changes(lines: &mut [Line]) {
    let mut at = 0;
    while at < lines.len() {
        if lines[at].kind != Kind::Removed {
            at += 1;
            continue;
        }
        let removed = at;
        while at < lines.len() && lines[at].kind == Kind::Removed {
            at += 1;
        }
        let added = at;
        while at < lines.len() && lines[at].kind == Kind::Added {
            at += 1;
        }
        let pairs = (added - removed).min(at - added);
        for n in 0..pairs {
            mark_change(lines, removed + n, added + n);
        }
    }
}

fn mark_change(lines: &mut [Line], old: usize, new: usize) {
    let before: Vec<char> = lines[old].text.chars().collect();
    let after: Vec<char> = lines[new].text.chars().collect();
    let start = before
        .iter()
        .zip(&after)
        .take_while(|(a, b)| a == b)
        .count();
    let room = before.len().min(after.len()) - start;
    let end = before
        .iter()
        .rev()
        .zip(after.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    // Lines with little in common were rewritten, not edited: marking all
    // of each says nothing.
    let longest = before.len().max(after.len());
    if (start + end) * 5 < longest * 2 {
        return;
    }
    if start < before.len() - end {
        lines[old].changed = Some((start, before.len() - end));
    }
    if start < after.len() - end {
        lines[new].changed = Some((start, after.len() - end));
    }
}

/// A file whose diff could not be read, for `why`.
pub fn failed(why: String) -> FileDiff {
    FileDiff {
        lines: vec![Line {
            text: why,
            ..note(Note::Failed)
        }],
        columns: 0,
        biggest: 0,
    }
}

fn hunk(text: String) -> Line {
    Line {
        kind: Kind::Hunk,
        old: None,
        new: None,
        text,
        changed: None,
    }
}

fn note(note: Note) -> Line {
    Line {
        kind: Kind::Note(note),
        old: None,
        new: None,
        text: String::new(),
        changed: None,
    }
}

/// A line as the panel shows it: tabs spread to their stops, what is not
/// printable shown as such, and no longer than a panel draws.
fn shown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut column = 0;
    for c in text.trim_end_matches('\r').chars().take(LINE_CHARS) {
        match c {
            '\t' => {
                let stop = TAB - column % TAB;
                out.extend(std::iter::repeat_n(' ', stop));
                column += stop;
            }
            c if c.is_control() => {
                out.push('\u{fffd}');
                column += 1;
            }
            c => {
                out.push(c);
                column += c.width().unwrap_or(0);
            }
        }
    }
    out
}

/// How many columns `text` takes.
pub fn columns(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// A file git would call binary: one with a NUL near its start.
fn binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|byte| *byte == 0)
}

fn count_lines(bytes: &[u8]) -> u32 {
    let newlines = bytes.iter().filter(|byte| **byte == b'\n').count();
    let unended = bytes.last().is_some_and(|byte| *byte != b'\n');
    (newlines + usize::from(unended)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,4 +10,5 @@ fn main() {
     let panel = Panel::new();
-    let rows = panel.rows();
+    let rows = panel.visible_rows();
+    rows.sort();

 \tprintln!(\"{rows:?}\");
\\ No newline at end of file
";

    #[test]
    fn a_unified_diff_reads_as_its_lines_with_their_numbers() {
        let diff = lines(DIFF.as_bytes(), false, false);
        let kinds: Vec<Kind> = diff.lines.iter().map(|line| line.kind).collect();
        assert_eq!(
            kinds,
            [
                Kind::Hunk,
                Kind::Context,
                Kind::Removed,
                Kind::Added,
                Kind::Added,
                Kind::Context,
                Kind::Context,
                Kind::Note(Note::NoNewline),
            ]
        );
        let numbers: Vec<(Option<u32>, Option<u32>)> =
            diff.lines.iter().map(|line| (line.old, line.new)).collect();
        assert_eq!(numbers[1], (Some(10), Some(10)));
        assert_eq!(numbers[2], (Some(11), None));
        assert_eq!(numbers[3], (None, Some(11)));
        assert_eq!(numbers[4], (None, Some(12)));
        assert_eq!(numbers[5], (Some(12), Some(13)), "an empty context line");
        assert_eq!(
            diff.lines[6].text, "    println!(\"{rows:?}\");",
            "a tab to its stop"
        );
        assert_eq!(diff.biggest, 14);
        // The edit is marked where it differs -- nothing was taken out of
        // the old line, `visible_` went into the new -- and the next new
        // line has nothing to pair with.
        assert_eq!(diff.lines[2].changed, None);
        assert_eq!(diff.lines[3].changed, Some((21, 29)));
        assert_eq!(diff.lines[4].changed, None);
    }

    #[test]
    fn a_diff_without_hunks_says_what_changed() {
        let binary = "diff --git a/logo.png b/logo.png\nindex 1..2 100644\nBinary files a/logo.png and b/logo.png differ\n";
        assert_eq!(
            lines(binary.as_bytes(), false, false).lines,
            [note(Note::Binary)]
        );
        let mode = "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n";
        assert_eq!(
            lines(mode.as_bytes(), false, false).lines,
            [note(Note::Mode)]
        );
        let renamed =
            "diff --git a/a.rs b/b.rs\nsimilarity index 100%\nrename from a.rs\nrename to b.rs\n";
        assert_eq!(
            lines(renamed.as_bytes(), false, false).lines,
            [note(Note::Unchanged)]
        );
    }

    #[test]
    fn a_new_file_is_all_added_and_a_long_one_is_cut() {
        let diff = lines(b"one\r\ntwo\n\x07three", false, true);
        assert_eq!(diff.lines[0].text, "@@ -0,0 +1,3 @@");
        let texts: Vec<&str> = diff.lines[1..]
            .iter()
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(texts, ["one", "two", "\u{fffd}three"]);
        assert!(diff.lines[1..].iter().all(|line| line.kind == Kind::Added));
        assert_eq!(lines(b"\x00\x01", false, true).lines, [note(Note::Binary)]);
        let cut = lines(b"a\nb\n", true, true);
        assert_eq!(cut.lines.last(), Some(&note(Note::Cut)));
    }

    #[test]
    fn rewritten_lines_are_not_marked_as_edits() {
        let text = "@@ -1,1 +1,1 @@\n-let frame = view.len();\n+return Ok(());\n";
        let diff = lines(text.as_bytes(), false, false);
        assert!(diff.lines.iter().all(|line| line.changed.is_none()));
    }

    #[test]
    fn the_lists_git_prints_become_changes_with_counts() {
        let statuses = b"M\0src/main.rs\0R087\0old/name.rs\0new/name.rs\0A\0logo.png\0";
        let counts = b"3\t1\tsrc/main.rs\x002\t2\t\0old/name.rs\0new/name.rs\0-\t-\tlogo.png\0";
        let files = changes(statuses, counts, b"");
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].counts, Some((3, 1)));
        assert_eq!(files[1].status, Status::Renamed);
        assert_eq!(files[1].from.as_deref(), Some("old/name.rs"));
        assert_eq!(files[1].path, "new/name.rs");
        assert_eq!(files[1].counts, Some((2, 2)));
        assert_eq!((files[2].status, files[2].counts), (Status::Added, None));

        // A name that is not UTF-8 keeps its place, and every name after it.
        let statuses = b"M\0caf\xe9.txt\0M\0src/a.rs\0";
        let counts = b"1\t0\tcaf\xe9.txt\x002\t0\tsrc/a.rs\0";
        let files = changes(statuses, counts, b"");
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["caf\u{fffd}.txt", "src/a.rs"]);
        assert_eq!(files[1].counts, Some((2, 0)));
    }

    #[test]
    fn a_line_shows_wide_characters_as_two_columns() {
        assert_eq!(columns("中文ab"), 6);
        assert_eq!(shown("a\tb"), "a   b");
        assert_eq!(shown("ab\tc"), "ab  c");
    }

    #[test]
    fn conflicts_keep_their_paths_and_counts_without_repeating_stages() {
        let files = changes(
            b"M\0a\tb\nc.txt\0",
            b"2\t1\ta\tb\nc.txt\0",
            b"100644 abc 1\ta\tb\nc.txt\0\
              100644 def 2\ta\tb\nc.txt\0\
              100644 abc 1\tmissing.txt\0\
              100644 def 3\tmissing.txt\0",
        );
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "a\tb\nc.txt");
        assert_eq!(files[0].status, Status::Conflicted);
        assert_eq!(files[0].counts, Some((2, 1)));
        assert_eq!(files[1].path, "missing.txt");
        assert_eq!(files[1].status, Status::Conflicted);
    }

    #[test]
    fn a_large_list_pairs_counts_by_path_in_any_order() {
        let count = FILES * 2;
        let statuses: Vec<u8> = (0..count)
            .flat_map(|n| format!("M\0file-{n:06}.txt\0").into_bytes())
            .collect();
        let counts: Vec<u8> = (0..count)
            .rev()
            .flat_map(|n| format!("{n}\t1\tfile-{n:06}.txt\0").into_bytes())
            .collect();
        let files = changes(&statuses, &counts, b"");
        assert_eq!(files.len(), count);
        for (n, file) in files.iter().enumerate() {
            assert_eq!(file.path, format!("file-{n:06}.txt"));
            assert_eq!(file.counts, Some((n as u32, 1)));
        }
    }

    /// Runs git in `dir` as an example user, with no hooks or signing.
    fn git_in(dir: &Path, args: &[&str]) -> Option<std::process::Output> {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=example",
                "-c",
                "user.email=user@example.com",
            ])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
    }

    /// A repository in a temporary directory, with a commit and changes
    /// since: when there is a git to make it with.
    fn repository() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().ok()?;
        let run = |args: &[&str]| git_in(dir.path(), args);
        run(&["init", "-q", "-b", "main"])?;
        std::fs::write(dir.path().join("kept.txt"), "one\ntwo\nthree\n").ok()?;
        std::fs::write(dir.path().join("gone.txt"), "bye\n").ok()?;
        run(&["add", "."])?;
        run(&["commit", "-q", "-m", "first"])?;
        std::fs::write(dir.path().join("kept.txt"), "one\n2\nthree\nfour\n").ok()?;
        std::fs::remove_file(dir.path().join("gone.txt")).ok()?;
        std::fs::write(dir.path().join("new.txt"), "hello\nworld\n").ok()?;
        Some(dir)
    }

    #[test]
    fn a_repository_is_found_with_its_changes() {
        let Some(dir) = repository() else {
            eprintln!("no git here: skipped");
            return;
        };
        let inner = dir.path().join("inside");
        std::fs::create_dir(&inner).unwrap();
        let inner = inner.to_str().unwrap();
        let Ok(Found::Repo(snapshot)) = look(&Here, inner, &mut Counted::default()) else {
            panic!("not found from a directory inside")
        };
        assert_eq!(snapshot.branch, "main");
        assert!(!snapshot.unborn);
        let seen: Vec<_> = snapshot
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.status, file.counts))
            .collect();
        assert_eq!(
            seen,
            [
                ("gone.txt", Status::Deleted, Some((0, 1))),
                ("kept.txt", Status::Modified, Some((2, 1))),
                ("new.txt", Status::Untracked, Some((2, 0))),
            ]
        );
        let (raw, cut) = diff(&Here, &snapshot, &snapshot.files[1]).unwrap();
        let kept = lines(&raw, cut, false);
        assert!(kept
            .lines
            .iter()
            .any(|line| line.kind == Kind::Added && line.text == "four"));
        let (raw, cut) = diff(&Here, &snapshot, &snapshot.files[2]).unwrap();
        assert_eq!(
            lines(&raw, cut, true).lines.len(),
            3,
            "a hunk and two lines"
        );

        let outside = tempfile::tempdir().unwrap();
        assert_eq!(
            look(
                &Here,
                outside.path().to_str().unwrap(),
                &mut Counted::default()
            ),
            Ok(Found::NotRepo)
        );
    }

    #[test]
    fn a_conflict_stays_listed_until_it_is_staged() {
        let Some(dir) = repository() else {
            eprintln!("no git here: skipped");
            return;
        };
        let root = dir.path();
        git_in(root, &["add", "."]).unwrap();
        git_in(root, &["commit", "-q", "-m", "base"]).unwrap();
        git_in(root, &["checkout", "-q", "-b", "other"]).unwrap();
        std::fs::write(root.join("kept.txt"), "other\n").unwrap();
        git_in(root, &["commit", "-qam", "other"]).unwrap();
        git_in(root, &["checkout", "-q", "main"]).unwrap();
        std::fs::write(root.join("kept.txt"), "ours\n").unwrap();
        git_in(root, &["commit", "-qam", "ours"]).unwrap();
        assert!(git_in(root, &["merge", "--no-edit", "other"]).is_none());
        let listed = || {
            let Ok(Found::Repo(snapshot)) =
                look(&Here, root.to_str().unwrap(), &mut Counted::default())
            else {
                panic!("no repository")
            };
            snapshot.files
        };
        let conflicted = || {
            let files = listed();
            assert_eq!(files.len(), 1, "one file, not one for each index stage");
            assert_eq!(files[0].path, "kept.txt");
            assert_eq!(files[0].status, Status::Conflicted);
        };
        conflicted();
        // The working tree matches HEAD, but the index is still unmerged.
        std::fs::write(root.join("kept.txt"), "ours\n").unwrap();
        assert_eq!(
            git_in(root, &["status", "--porcelain"]).unwrap().stdout,
            b"UU kept.txt\n"
        );
        conflicted();
        std::fs::remove_file(root.join("kept.txt")).unwrap();
        conflicted();
        std::fs::write(root.join("kept.txt"), "ours\n").unwrap();
        git_in(root, &["add", "kept.txt"]).unwrap();
        assert!(listed().is_empty());
    }

    /// A name with a pattern's characters in it is only that name, and a
    /// new link shows where it points, not what is there.
    #[cfg(unix)]
    #[test]
    fn a_name_is_a_name_and_a_link_is_where_it_points() {
        let Some(dir) = repository() else {
            eprintln!("no git here: skipped");
            return;
        };
        let root = dir.path();
        std::fs::write(root.join("a*.txt"), "star\n").unwrap();
        std::fs::write(root.join("ab.txt"), "plain\n").unwrap();
        git_in(root, &["add", "."]).unwrap();
        git_in(root, &["commit", "-q", "-m", "second"]).unwrap();
        std::fs::write(root.join("a*.txt"), "star 2\n").unwrap();
        std::fs::write(root.join("ab.txt"), "plain 2\n").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        std::fs::write(&secret, "not to be shown\n").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("link")).unwrap();

        let Ok(Found::Repo(snapshot)) =
            look(&Here, root.to_str().unwrap(), &mut Counted::default())
        else {
            panic!("no repository")
        };
        let texts = |file: &Change, untracked: bool| -> Vec<String> {
            let (raw, cut) = diff(&Here, &snapshot, file).unwrap();
            lines(&raw, cut, untracked)
                .lines
                .into_iter()
                .map(|line| line.text)
                .collect()
        };
        let star = snapshot.files.iter().find(|file| file.path == "a*.txt");
        let star = texts(star.unwrap(), false);
        assert!(star.contains(&"star 2".to_string()), "{star:?}");
        assert!(!star.iter().any(|text| text.contains("plain")), "{star:?}");

        let link = snapshot.files.iter().find(|file| file.path == "link");
        let link = link.unwrap();
        assert_eq!(link.counts, Some((1, 0)));
        let shown = texts(link, true);
        assert_eq!(shown[1], secret.display().to_string());
        assert!(!shown.iter().any(|text| text.contains("not to be shown")));
    }

    /// Another machine, as the plugin sees one: git answers from a table,
    /// and every stat and read is counted.
    struct Far {
        asked: std::cell::RefCell<Vec<String>>,
    }

    impl Machine for Far {
        fn git(&self, dir: &str, args: &[&str], _limit: usize) -> Result<(Vec<u8>, bool), Failure> {
            self.asked
                .borrow_mut()
                .push(format!("git {dir} {}", args.join(" ")));
            let out: &[u8] = match args {
                ["rev-parse", "--show-toplevel"] => b"/srv/app\n",
                ["rev-parse", "--verify", ..] => b"abc\n",
                ["symbolic-ref", ..] => b"main\n",
                ["diff", _, "--name-status", ..] => b"",
                ["diff", _, "--numstat", ..] => b"",
                ["ls-files", "--unmerged", ..] => b"",
                ["ls-files", ..] => b"new.txt\0",
                _ => return Err(Failure::Refused),
            };
            Ok((out.to_vec(), false))
        }

        fn stat(&self, path: &str) -> Option<Meta> {
            self.asked.borrow_mut().push(format!("stat {path}"));
            Some(Meta {
                len: 3,
                modified: None,
                file: true,
                link: None,
            })
        }

        fn read(&self, path: &str, _limit: usize) -> Result<(Vec<u8>, bool), String> {
            self.asked.borrow_mut().push(format!("read {path}"));
            Ok((b"hi\n".to_vec(), false))
        }

        fn far(&self) -> bool {
            true
        }
    }

    #[test]
    fn on_another_machine_new_files_are_listed_but_not_counted() {
        let far = Far {
            asked: Default::default(),
        };
        let Ok(Found::Repo(snapshot)) = look(&far, "/srv/app/src", &mut Counted::default()) else {
            panic!("no repository")
        };
        assert_eq!(snapshot.root, "/srv/app");
        assert_eq!(snapshot.files[0].path, "new.txt");
        assert_eq!(snapshot.files[0].counts, None, "a trip a file, every look");
        assert!(!far
            .asked
            .borrow()
            .iter()
            .any(|asked| !asked.starts_with("git")));
        let (raw, _) = diff(&far, &snapshot, &snapshot.files[0]).unwrap();
        assert_eq!(raw, b"hi\n");
        assert_eq!(
            far.asked.borrow()[far.asked.borrow().len() - 2..],
            ["stat /srv/app/new.txt", "read /srv/app/new.txt"]
        );
        assert_eq!(joined("/", "a"), "/a");
    }

    #[test]
    fn a_file_changed_within_the_second_to_the_same_size_is_counted_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        let path = path.to_str().unwrap();
        let second = std::time::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        let write = |text: &str, at: SystemTime| {
            std::fs::write(path, text).unwrap();
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_modified(at).unwrap();
        };
        let mut counted = Counted::default();
        write("a\nb\n", second);
        assert_eq!(counted.lines(&Here, path, &mut 1), Some(2));
        write("abcd", second + Duration::from_millis(300));
        assert_eq!(
            counted.lines(&Here, path, &mut 1),
            Some(1),
            "not the count from before"
        );
    }
}
