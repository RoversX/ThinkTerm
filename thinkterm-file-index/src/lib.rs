//! Walking a project for file search. The desktop indexes a local project with
//! these rules, and a remote project is indexed by `thinkterm list-files` on
//! the remote host with the same ones, so both find the same files.

use ignore::{DirEntry, WalkBuilder};
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

/// The most entries an index holds; a walk stops there.
pub const ENTRY_LIMIT: usize = 100_000;

/// Directories that are never indexed or browsed into: version control,
/// build output and dependency trees.
pub fn should_skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".hg"
            | ".svn"
            | "target"
            | "node_modules"
            | ".next"
            | ".nuxt"
            | ".turbo"
            | ".cache"
            | "dist"
            | "build"
            | "coverage"
            | "vendor"
            | ".venv"
            | "venv"
            | "__pycache__"
    )
}

fn should_walk_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_dir()) {
        return true;
    }
    !should_skip_dir(&entry.file_name().to_string_lossy())
}

/// A walker over `root` under the project rules. `ignore` is ripgrep's
/// walker: it prunes ignored directories as it goes rather than listing then
/// discarding them, which is what keeps a project with a large vendored
/// subtree cheap. The first entry it yields is `root` itself.
pub fn project_walker(root: &Path, respect_gitignore: bool) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        // Dotfiles stay visible -- `.github`, `.cargo` and friends are part of
        // the project. Only .gitignore decides what is hidden.
        .hidden(false)
        .follow_links(false)
        .git_ignore(respect_gitignore)
        .git_exclude(respect_gitignore)
        // Only this project's own ignore rules: no global core.excludesFile and
        // no walking up into parent repositories.
        .git_global(false)
        .parents(false)
        // Honour .gitignore even in a directory that is not a git repo yet.
        .require_git(false)
        .filter_entry(should_walk_entry);
    builder
}

/// `path` relative to nothing in particular, with `/` between components
/// whatever the platform, as search results display it.
pub fn display_path(path: &Path) -> String {
    let mut display = String::new();
    for component in path.components() {
        if !display.is_empty() {
            display.push('/');
        }
        display.push_str(&component.as_os_str().to_string_lossy());
    }
    display
}

/// The listing `thinkterm list-files` writes: a header line, then one record
/// per entry -- `d` or `f`, the path relative to the root, NUL -- and a final
/// record saying whether the walk finished. A stream cut short has no final
/// record, so it cannot be mistaken for a complete one.
const HEADER: &[u8] = b"thinkterm-file-index 1\n";
const COMPLETE: &[u8] = b"!complete\0";
const TRUNCATED: &[u8] = b"!truncated\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedEntry {
    /// Relative to the root, `/`-separated.
    pub path: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Listing {
    pub entries: Vec<ListedEntry>,
    /// The walk stopped at a limit, so some files are missing.
    pub truncated: bool,
}

/// Walk `root`, stopping at `limit` entries or at `deadline`, whichever comes
/// first. Unreadable entries are skipped, as the local index skips them.
pub fn list_project(
    root: &Path,
    respect_gitignore: bool,
    limit: usize,
    deadline: Instant,
) -> io::Result<Listing> {
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the project folder is not a directory",
        ));
    }
    let mut listing = Listing::default();
    for entry in project_walker(root, respect_gitignore).build().skip(1) {
        if listing.entries.len() >= limit || Instant::now() >= deadline {
            listing.truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let path = display_path(relative);
        // A NUL cannot appear in a file name, but a lossy conversion must not
        // be able to smuggle one into the listing either.
        if path.is_empty() || path.contains('\0') {
            continue;
        }
        listing.entries.push(ListedEntry {
            path,
            is_dir: entry.file_type().is_some_and(|kind| kind.is_dir()),
        });
    }
    Ok(listing)
}

/// Walk `root` as `list_project` does and write the listing to `out`.
pub fn write_listing(
    root: &Path,
    respect_gitignore: bool,
    limit: usize,
    deadline: Instant,
    out: &mut impl Write,
) -> io::Result<()> {
    let listing = list_project(root, respect_gitignore, limit, deadline)?;
    out.write_all(HEADER)?;
    for entry in &listing.entries {
        out.write_all(if entry.is_dir { b"d" } else { b"f" })?;
        out.write_all(entry.path.as_bytes())?;
        out.write_all(b"\0")?;
    }
    out.write_all(if listing.truncated {
        TRUNCATED
    } else {
        COMPLETE
    })?;
    out.flush()
}

/// Read a listing from the output of a command that wrote one, which may
/// start with whatever a shell startup file printed first. The listing
/// begins at the first line that is its header.
pub fn parse_command_output(bytes: &[u8]) -> Result<Listing, String> {
    let start = if bytes.starts_with(HEADER) {
        Some(0)
    } else {
        bytes
            .windows(HEADER.len() + 1)
            .position(|window| window[0] == b'\n' && &window[1..] == HEADER)
            .map(|at| at + 1)
    };
    let start = start.ok_or_else(|| "not a file listing".to_string())?;
    parse_listing(&bytes[start..])
}

/// Read a listing written by [`write_listing`]. Refuses anything else,
/// including a listing without its final record.
pub fn parse_listing(bytes: &[u8]) -> Result<Listing, String> {
    let body = bytes
        .strip_prefix(HEADER)
        .ok_or_else(|| "not a file listing".to_string())?;
    let mut listing = Listing::default();
    let mut rest = body;
    loop {
        let Some(end) = rest.iter().position(|&b| b == 0) else {
            return Err("the file listing ended early".to_string());
        };
        let record = &rest[..=end];
        rest = &rest[end + 1..];
        match record.first() {
            Some(b'!') => {
                listing.truncated = match record {
                    COMPLETE => false,
                    TRUNCATED => true,
                    _ => return Err("unknown file listing trailer".to_string()),
                };
                if !rest.is_empty() {
                    return Err("data after the end of the file listing".to_string());
                }
                return Ok(listing);
            }
            Some(kind @ (b'd' | b'f')) => {
                let path = std::str::from_utf8(&record[1..record.len() - 1])
                    .map_err(|_| "a file listing path is not UTF-8".to_string())?;
                if path.is_empty() {
                    return Err("an empty path in the file listing".to_string());
                }
                listing.entries.push(ListedEntry {
                    path: path.to_string(),
                    is_dir: *kind == b'd',
                });
            }
            _ => return Err("an unknown file listing record".to_string()),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(root.join(".github")).unwrap();
        fs::create_dir_all(root.join("ignored")).unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();
        fs::write(root.join("src/nested/deep.md"), "").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "").unwrap();
        fs::write(root.join(".github/ci.yml"), "").unwrap();
        fs::write(root.join("ignored/secret.txt"), "").unwrap();
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        fs::write(root.join("README.md"), "").unwrap();
        dir
    }

    fn listed(root: &Path, gitignore: bool, limit: usize) -> Listing {
        let mut out = vec![];
        write_listing(
            root,
            gitignore,
            limit,
            Instant::now() + Duration::from_secs(60),
            &mut out,
        )
        .unwrap();
        parse_listing(&out).unwrap()
    }

    fn paths(listing: &Listing) -> Vec<String> {
        let mut paths: Vec<_> = listing.entries.iter().map(|e| e.path.clone()).collect();
        paths.sort();
        paths
    }

    #[test]
    fn listing_follows_the_project_rules() {
        let dir = tree();
        let listing = listed(dir.path(), true, ENTRY_LIMIT);
        assert!(!listing.truncated);
        assert_eq!(
            paths(&listing),
            vec![
                ".github",
                ".github/ci.yml",
                ".gitignore",
                "README.md",
                "src",
                "src/main.rs",
                "src/nested",
                "src/nested/deep.md",
            ]
        );
        let src = listing.entries.iter().find(|e| e.path == "src").unwrap();
        assert!(src.is_dir);
        let without_gitignore = listed(dir.path(), false, ENTRY_LIMIT);
        assert!(paths(&without_gitignore).contains(&"ignored/secret.txt".to_string()));
    }

    #[test]
    fn listing_stops_at_the_limit_and_says_so() {
        let dir = tree();
        let listing = listed(dir.path(), true, 3);
        assert_eq!(listing.entries.len(), 3);
        assert!(listing.truncated);
    }

    #[test]
    fn listing_stops_at_the_deadline() {
        let dir = tree();
        let mut out = vec![];
        write_listing(dir.path(), true, ENTRY_LIMIT, Instant::now(), &mut out).unwrap();
        let listing = parse_listing(&out).unwrap();
        assert!(listing.entries.is_empty());
        assert!(listing.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn listing_does_not_follow_links_out_of_the_root() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("elsewhere.txt"), "").unwrap();
        let dir = tree();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let listing = listed(dir.path(), true, ENTRY_LIMIT);
        let names = paths(&listing);
        assert!(names.contains(&"link".to_string()));
        assert!(!names.iter().any(|p| p.starts_with("link/")));
    }

    #[test]
    fn a_listing_must_be_whole() {
        let dir = tree();
        let mut out = vec![];
        write_listing(
            dir.path(),
            true,
            ENTRY_LIMIT,
            Instant::now() + Duration::from_secs(60),
            &mut out,
        )
        .unwrap();
        assert!(parse_listing(&out[..out.len() - 1]).is_err());
        assert!(parse_listing(&out[..out.len() / 2]).is_err());
        assert!(parse_listing(b"something else\n").is_err());
        let mut extra = out.clone();
        extra.extend_from_slice(b"fmore\0");
        assert!(parse_listing(&extra).is_err());
        assert!(parse_listing(b"thinkterm-file-index 1\nx\0!complete\0").is_err());
        assert!(parse_listing(b"thinkterm-file-index 1\nf\0!complete\0").is_err());
    }

    #[test]
    fn command_output_may_start_with_a_greeting() {
        let dir = tree();
        let mut out = b"welcome back\nthinkterm-file-index is great\n".to_vec();
        write_listing(
            dir.path(),
            true,
            ENTRY_LIMIT,
            Instant::now() + Duration::from_secs(60),
            &mut out,
        )
        .unwrap();
        let listing = parse_command_output(&out).unwrap();
        assert!(paths(&listing).contains(&"README.md".to_string()));
        assert!(parse_listing(&out).is_err());
        assert!(parse_command_output(b"welcome back\n").is_err());
    }

    #[test]
    fn a_missing_root_is_an_error() {
        let dir = tree();
        let mut out = vec![];
        assert!(write_listing(
            &dir.path().join("absent"),
            true,
            ENTRY_LIMIT,
            Instant::now() + Duration::from_secs(60),
            &mut out
        )
        .is_err());
        assert!(out.is_empty());
    }
}
