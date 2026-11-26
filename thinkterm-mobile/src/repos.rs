//! Threads by repository: the projects of every connected host put
//! together by the git repository their folders are in, for the Threads
//! home's "by project" view. The shells hand over what their hosts' trees
//! say; the grouping is here once for both.

use std::collections::HashMap;

/// A project of one host whose folder is in a repository with a remote.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct RepoProject {
    /// The shell's id for the host.
    pub host: String,
    pub space: String,
    pub project: String,
    /// `github.com/owner/repo`, as the host's server read it.
    pub remote: String,
    pub branch: Option<String>,
    /// The linked worktree's folder, when it is in one.
    pub worktree: Option<String>,
}

/// One repository and its projects on every host.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct RepoGroup {
    /// The repository's own name: the remote's last part.
    pub name: String,
    pub remote: String,
    /// The remote's path before the name, without its host: the owner, or
    /// a group and its subgroups; empty when there is none.
    pub owner: String,
    /// Another group has the same owner and name on another host: only the
    /// host tells the two apart.
    pub host_needed: bool,
    pub projects: Vec<RepoProject>,
}

/// The repositories `projects` are in, by name. Two remotes that differ
/// only in case are one: the large hosts treat them so. Within one, the
/// projects keep their hosts' order, each host's own checkout before its
/// worktrees.
#[uniffi::export]
pub fn repo_groups(projects: Vec<RepoProject>) -> Vec<RepoGroup> {
    let mut host_order: HashMap<String, usize> = HashMap::new();
    for project in &projects {
        let next = host_order.len();
        host_order.entry(project.host.clone()).or_insert(next);
    }
    let mut groups: Vec<RepoGroup> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for project in projects {
        let remote = project.remote.trim().trim_end_matches('/');
        if remote.is_empty() {
            continue;
        }
        let key = remote.to_lowercase();
        let at = *index.entry(key).or_insert_with(|| {
            let path = remote.split_once('/').map_or("", |(_, path)| path);
            groups.push(RepoGroup {
                name: remote.rsplit('/').next().unwrap_or(remote).to_string(),
                remote: remote.to_string(),
                owner: path.rsplit_once('/').map_or("", |(owner, _)| owner).to_string(),
                host_needed: false,
                projects: Vec::new(),
            });
            groups.len() - 1
        });
        groups[at].projects.push(project);
    }
    let mut paths: HashMap<String, usize> = HashMap::new();
    for group in &groups {
        *paths.entry(path_of(group)).or_default() += 1;
    }
    for group in &mut groups {
        group.host_needed = paths.get(&path_of(group)).is_some_and(|&n| n > 1);
    }
    for group in &mut groups {
        group
            .projects
            .sort_by_key(|p| (host_order.get(&p.host).copied(), p.worktree.is_some()));
    }
    groups.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.remote.to_lowercase().cmp(&b.remote.to_lowercase()))
    });
    groups
}

/// A group's remote without its host, as two hosts' would compare.
fn path_of(group: &RepoGroup) -> String {
    group.remote.split_once('/').map_or("", |(_, path)| path).to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(host: &str, id: &str, remote: &str, worktree: Option<&str>) -> RepoProject {
        RepoProject {
            host: host.into(),
            space: "s".into(),
            project: id.into(),
            remote: remote.into(),
            branch: Some("main".into()),
            worktree: worktree.map(Into::into),
        }
    }

    #[test]
    fn one_repository_on_two_hosts_is_one_group() {
        let groups = repo_groups(vec![
            project("server", "a", "github.com/RoversX/ThinkTerm", Some("feature")),
            project("mac", "blog", "github.com/me/blog", None),
            project("server", "b", "github.com/RoversX/ThinkTerm", None),
            project("mac", "c", "github.com/roversx/thinkterm", None),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "blog");
        assert_eq!(groups[1].name, "ThinkTerm");
        let order: Vec<&str> = groups[1].projects.iter().map(|p| p.project.as_str()).collect();
        // The server came first; its own checkout before its worktree.
        assert_eq!(order, ["b", "a", "c"]);
        assert_eq!((groups[0].owner.as_str(), groups[0].host_needed), ("me", false));
        assert_eq!((groups[1].owner.as_str(), groups[1].host_needed), ("RoversX", false));
    }

    #[test]
    fn the_host_is_needed_only_where_it_tells_two_apart() {
        let groups = repo_groups(vec![
            project("server", "a", "github.com/me/Blog", None),
            project("server", "b", "gitlab.com/me/blog", None),
            project("server", "c", "git.example.org/group/sub/tool", None),
            project("server", "d", "git.example.org/solo", None),
        ]);
        let shown: Vec<(&str, &str, bool)> =
            groups.iter().map(|g| (g.remote.as_str(), g.owner.as_str(), g.host_needed)).collect();
        assert_eq!(
            shown,
            [
                ("github.com/me/Blog", "me", true),
                ("gitlab.com/me/blog", "me", true),
                ("git.example.org/solo", "", false),
                ("git.example.org/group/sub/tool", "group/sub", false),
            ]
        );
    }

    #[test]
    fn a_project_without_a_remote_is_in_none() {
        let groups = repo_groups(vec![project("server", "notes", " ", None)]);
        assert!(groups.is_empty());
    }
}
