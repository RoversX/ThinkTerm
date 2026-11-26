//! What a folder's git checkout is, read from the files git keeps rather
//! than by running git: the folder it is checked out in, whether that is a
//! linked worktree, the branch there, and where the repository comes from.
//!
//! Reading can block -- a directory on a stalled network mount, a file not
//! yet downloaded -- so callers read on a thread of their own.

use std::io::Read;
use std::path::{Path, PathBuf};

/// A git checkout, as its files tell it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkout {
    /// The folder it is checked out in: the one holding `.git`.
    pub folder: PathBuf,
    /// A linked worktree (`git worktree add`), not the repository's own
    /// checkout.
    pub linked: bool,
    /// The branch checked out, or the start of the commit when none is.
    pub branch: Option<String>,
    /// Where the repository comes from, the same however it was cloned:
    /// `github.com/owner/repo`. See [`remote_name`].
    pub remote: Option<String>,
}

/// The longest file read here. Git writes `HEAD`, `.git` and `commondir`
/// far shorter; a config with many branches runs longer.
const MAX_FILE: u64 = 64 * 1024;

/// The checkout `dir` is in: the nearest folder at or above it with a
/// `.git`. A repository's own checkout keeps a directory there; a linked
/// worktree, a submodule or a separated git directory keep a file naming
/// the git directory, and only a linked worktree's points back at the
/// repository through a `commondir`.
pub fn checkout_of(dir: &Path) -> Option<Checkout> {
    for folder in dir.ancestors() {
        let dot_git = folder.join(".git");
        let Ok(meta) = std::fs::metadata(&dot_git) else {
            continue;
        };
        let (git_dir, common_dir, linked) = if meta.is_dir() {
            (dot_git.clone(), dot_git, false)
        } else if meta.is_file() {
            let text = read_small_file(&dot_git)?;
            let git_dir = folder.join(text.lines().next()?.strip_prefix("gitdir:")?.trim());
            match read_small_file(&git_dir.join("commondir")) {
                Some(common) => {
                    let common = git_dir.join(common.lines().next()?.trim());
                    (git_dir, common, true)
                }
                None => (git_dir.clone(), git_dir, false),
            }
        } else {
            return None;
        };
        let branch = read_small_file(&git_dir.join("HEAD")).and_then(|head| branch_of_head(&head));
        let remote = read_small_file(&common_dir.join("config"))
            .and_then(|config| remote_url(&config))
            .and_then(|url| remote_name(&url));
        return Some(Checkout {
            folder: folder.to_path_buf(),
            linked,
            branch,
            remote,
        });
    }
    None
}

/// The text of a file git keeps: only a plain one -- a FIFO would block the
/// read, a device would not end it -- and no longer than [`MAX_FILE`].
fn read_small_file(path: &Path) -> Option<String> {
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_FILE)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// The branch a `HEAD` file names, or the start of the commit it holds when
/// detached. A reftable repository keeps its real HEAD elsewhere and leaves
/// `refs/heads/.invalid` in the file, which is no branch.
pub fn branch_of_head(head: &str) -> Option<String> {
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(reference) => {
            let branch = reference.strip_prefix("refs/heads/").unwrap_or(reference);
            (branch != ".invalid").then(|| branch.to_string())
        }
        None => (head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| head[..7].to_string()),
    }
}

/// The url of the repository's `origin`, or of its first remote when it
/// has none by that name, from the text of its `config`.
fn remote_url(config: &str) -> Option<String> {
    let mut remote: Option<String> = None;
    let mut first = None;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            remote = remote_section(line);
            continue;
        }
        let Some(name) = remote.as_deref() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("url") {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value)
            .to_string();
        if name == "origin" {
            return Some(value);
        }
        first.get_or_insert(value);
    }
    first
}

/// The remote a config section header names: `[remote "origin"]`, or the
/// older `[remote.origin]`.
fn remote_section(header: &str) -> Option<String> {
    let inner = header.strip_prefix('[')?.split(']').next()?.trim();
    if let Some(rest) = inner.strip_prefix("remote") {
        let rest = rest.trim();
        if let Some(name) = rest.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
            return Some(name.to_string());
        }
        if let Some(name) = rest.strip_prefix('.') {
            return Some(name.to_string());
        }
    }
    None
}

/// A remote's url as `host/path`, the same for every way of cloning one
/// repository: `git@github.com:Owner/Repo.git`,
/// `ssh://git@github.com:22/Owner/Repo` and
/// `https://user:token@github.com/Owner/Repo.git` are all
/// `github.com/Owner/Repo`. The user, password, port, query and `.git` are
/// left out, so a token in the url never goes further. A remote on this
/// machine -- a path, `file://`, `localhost` or a loopback address -- names
/// nothing another machine shares, and is `None`.
pub fn remote_name(url: &str) -> Option<String> {
    let url = url.trim();
    let (host, path) = match url.split_once("://") {
        Some((scheme, rest)) => {
            let scheme = scheme.to_ascii_lowercase();
            if !matches!(
                scheme.as_str(),
                "ssh" | "git+ssh" | "ssh+git" | "git" | "http" | "https"
            ) {
                return None;
            }
            let rest = rest.split(['?', '#']).next()?;
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
            (strip_port(host), path)
        }
        None => {
            // scp-like `[user@]host:path`. A colon after a slash, or none,
            // is a path on this machine; so is `C:\…`.
            let (before, path) = url.split_once(':')?;
            if before.contains('/') || before.contains('\\') || before.len() < 2 {
                return None;
            }
            let host = before.rsplit_once('@').map_or(before, |(_, host)| host);
            (host, path)
        }
    };
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if host.is_empty() || host.contains(char::is_whitespace) || on_this_machine(&host) {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path).trim_end_matches('/');
    if path.is_empty() {
        return None;
    }
    Some(format!("{host}/{path}"))
}

/// A host that is every machine's own: each one's `localhost` is a
/// different server.
fn on_this_machine(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Ok(std::net::IpAddr::V6(ip)) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback() || ip.is_unspecified())
        }
        Err(_) => false,
    }
}

/// A host without its port: `host:22`, `[::1]:22`.
fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host.split_once(']').map_or(host, |(inner, _)| &host[..inner.len() + 1]);
    }
    match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_head_names_its_branch_or_its_commit() {
        assert_eq!(
            branch_of_head("ref: refs/heads/main\n").as_deref(),
            Some("main")
        );
        assert_eq!(
            branch_of_head("ref: refs/heads/feature/login\r\n").as_deref(),
            Some("feature/login")
        );
        assert_eq!(
            branch_of_head("a1b2c3d4e5f60718\n").as_deref(),
            Some("a1b2c3d")
        );
        assert_eq!(branch_of_head("ref: refs/heads/.invalid\n"), None);
        assert_eq!(branch_of_head("not a head"), None);
    }

    #[test]
    fn every_way_of_cloning_names_the_same_repository() {
        for url in [
            "git@github.com:RoversX/ThinkTerm.git",
            "github.com:RoversX/ThinkTerm",
            "ssh://git@github.com/RoversX/ThinkTerm.git",
            "ssh://git@github.com:22/RoversX/ThinkTerm/",
            "git+ssh://git@GitHub.com/RoversX/ThinkTerm",
            "https://github.com/RoversX/ThinkTerm.git",
            "https://user:ghp_secret@github.com/RoversX/ThinkTerm.git",
            "https://ghp_secret@github.com:443/RoversX/ThinkTerm?x=1#y",
            "git://github.com/RoversX/ThinkTerm",
        ] {
            assert_eq!(
                remote_name(url).as_deref(),
                Some("github.com/RoversX/ThinkTerm"),
                "{url}"
            );
        }
        assert_eq!(
            remote_name("git@gitlab.example.org:group/sub/project.git").as_deref(),
            Some("gitlab.example.org/group/sub/project")
        );
    }

    #[test]
    fn a_remote_on_this_machine_names_nothing() {
        for url in [
            "/srv/git/repo.git",
            "../repo",
            "./repo:with-colon",
            "file:///srv/git/repo.git",
            "C:\\repos\\repo",
            "C:/repos/repo",
            "https://github.com/",
            "",
            // Every machine's own server, a different one on each.
            "ssh://git@[::1]:2222/repo.git",
            "git@localhost:me/app.git",
            "http://127.0.0.1:3000/me/app",
            "https://LOCALHOST/me/app",
            "https://git.localhost/me/app",
            "http://[::ffff:127.0.0.1]:3000/me/app",
            "http://0.0.0.0:3000/me/app",
        ] {
            assert_eq!(remote_name(url), None, "{url}");
        }
    }

    #[test]
    fn origin_wins_over_the_first_remote() {
        let config = "[core]\n\tbare = false\n\
            [remote \"upstream\"]\n\turl = https://github.com/other/repo.git\n\
            [branch \"main\"]\n\tremote = origin\n\
            [remote \"origin\"]\n\tURL = \"git@github.com:me/repo.git\"\n";
        assert_eq!(
            remote_url(config).as_deref(),
            Some("git@github.com:me/repo.git")
        );
        let config = "[remote.fork]\n  url = git@host:fork/repo\n";
        assert_eq!(remote_url(config).as_deref(), Some("git@host:fork/repo"));
        assert_eq!(remote_url("[core]\n\turl = nothing\n"), None);
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_checkout_and_its_linked_worktree_share_their_remote() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        write(&repo.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(
            &repo.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://me:token@github.com/me/repo.git\n",
        );
        let tree = repo.join(".git/worktrees/feature");
        write(&tree.join("HEAD"), "ref: refs/heads/feature\n");
        write(&tree.join("commondir"), "../..\n");
        let linked = root.path().join("feature");
        write(
            &linked.join(".git"),
            &format!("gitdir: {}\n", tree.display()),
        );
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();

        let main = checkout_of(&repo.join("src/deep")).unwrap();
        assert_eq!(main.folder, repo);
        assert!(!main.linked);
        assert_eq!(main.branch.as_deref(), Some("main"));
        assert_eq!(main.remote.as_deref(), Some("github.com/me/repo"));

        let feature = checkout_of(&linked).unwrap();
        assert_eq!(feature.folder, linked);
        assert!(feature.linked);
        assert_eq!(feature.branch.as_deref(), Some("feature"));
        assert_eq!(feature.remote.as_deref(), Some("github.com/me/repo"));
    }

    #[test]
    fn a_submodule_has_its_own_remote_and_is_no_worktree() {
        let root = tempfile::tempdir().unwrap();
        let modules = root.path().join("parent/.git/modules/lib");
        write(&modules.join("HEAD"), "0123456789abcdef0123\n");
        write(
            &modules.join("config"),
            "[remote \"origin\"]\n\turl = git@github.com:someone/lib\n",
        );
        let lib = root.path().join("parent/lib");
        write(&lib.join(".git"), "gitdir: ../.git/modules/lib\n");
        let checkout = checkout_of(&lib).unwrap();
        assert!(!checkout.linked);
        assert_eq!(checkout.branch.as_deref(), Some("0123456"));
        assert_eq!(checkout.remote.as_deref(), Some("github.com/someone/lib"));
    }

    #[test]
    fn a_folder_outside_any_checkout_has_none() {
        let root = tempfile::tempdir().unwrap();
        // tempdir is not in a checkout on any machine this runs on; if it
        // were, the answer would name that one, never this folder.
        let found = checkout_of(root.path());
        assert!(found.map_or(true, |c| c.folder != root.path()));
    }
}
