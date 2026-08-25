//! Agent-aware foreground selection for Windows, ported from herdr's
//! design: no tty foreground group exists there, so walk the pane's
//! process tree in a shared Toolhelp32 snapshot, identify every
//! candidate, and let the topmost-ancestor agent win — claude beats the
//! node MCP child it spawned. The youngest-console heuristic behind
//! `get_foreground_process_name` keeps serving every non-agent consumer
//! (tab titles, close prompts) untouched.
//!
//! The selection core is pure and host-tested; only the fact-gathering
//! glue is `cfg(windows)`. Deferred, deliberately: git-bash re-parents
//! processes out of the shell's subtree (herdr tags panes with an env
//! marker and scans machine-wide), and PEB-environment identity hints.

use procinfo::ProcessTableEntry;

/// Pick the agent pid for a pane rooted at `root`. `identify` is called
/// once per tree member (root included) with its table entry and returns
/// the manifest id when that process is an agent; the callback decides
/// how much it wants to pay per process (name first, argv on demand).
///
/// Selection: the candidate that is an ancestor of every other candidate
/// wins. Candidates that form no single chain mean the tree is ambiguous
/// — return `None` and let the caller fall back, matching herdr.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn select_agent(
    entries: &[ProcessTableEntry],
    root: u32,
    identify: &mut dyn FnMut(&ProcessTableEntry) -> Option<String>,
) -> Option<(u32, String)> {
    let mut children: std::collections::HashMap<u32, Vec<&ProcessTableEntry>> =
        std::collections::HashMap::new();
    let mut by_pid: std::collections::HashMap<u32, &ProcessTableEntry> =
        std::collections::HashMap::new();
    for entry in entries {
        children.entry(entry.ppid).or_default().push(entry);
        by_pid.insert(entry.pid, entry);
    }

    // The tree under (and including) the root. Pid reuse can fabricate
    // parent links that loop; the visited set bounds the walk.
    let mut members: Vec<&ProcessTableEntry> = Vec::new();
    let mut visited: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut queue: Vec<u32> = vec![root];
    while let Some(pid) = queue.pop() {
        if !visited.insert(pid) {
            continue;
        }
        if let Some(entry) = by_pid.get(&pid) {
            members.push(entry);
        }
        if let Some(kids) = children.get(&pid) {
            queue.extend(kids.iter().map(|kid| kid.pid));
        }
    }

    let mut candidates: Vec<(&ProcessTableEntry, String)> = Vec::new();
    for entry in &members {
        if let Some(agent) = identify(entry) {
            candidates.push((entry, agent));
        }
    }
    if candidates.is_empty() {
        return None;
    }

    // Ancestor-of-all wins. Ancestry is checked inside the member set
    // only, again visit-bounded against pid-reuse loops.
    let is_ancestor = |ancestor: u32, mut pid: u32| -> bool {
        let mut seen = std::collections::HashSet::new();
        while pid != ancestor {
            if !seen.insert(pid) {
                return false;
            }
            match by_pid.get(&pid) {
                Some(entry) if visited.contains(&entry.ppid) && entry.ppid != entry.pid => {
                    pid = entry.ppid;
                }
                _ => return false,
            }
        }
        true
    };
    candidates
        .iter()
        .find(|(top, _)| {
            candidates
                .iter()
                .all(|(other, _)| is_ancestor(top.pid, other.pid))
        })
        .map(|(entry, agent)| (entry.pid, agent.clone()))
}

/// Windows replacement for the unix leader probe: tree-based agent
/// selection first, and every miss falls back to the youngest-console
/// heuristic behind `get_foreground_process_name`, so behavior is never
/// worse than before this module existed. Returns `(leader_path, agent)`
/// as `identify::probe` caches them; with an agent selected, the leader
/// path is that agent's executable, which also gives the contract's
/// leader-equality liveness check a real signal on Windows.
#[cfg(windows)]
pub(crate) fn probe_pane(
    pane: &dyn crate::pane::Pane,
    policy: crate::pane::CachePolicy,
) -> (Option<String>, Option<String>) {
    if let (Some(root), Some(table)) = (pane.root_process_id(), facts::shared_process_table()) {
        let lookup = |name: &str| super::engine::manifest_id_for_alias(name);
        let mut identify_entry = |entry: &ProcessTableEntry| {
            // Name first (free); argv — a PEB read — only when the name
            // is a generic runtime.
            let mut argv = || procinfo::LocalProcessInfo::argv_for_pid(entry.pid);
            super::proc_match::identify(&entry.name, &mut argv, &lookup)
        };
        if let Some((pid, agent)) = select_agent(table.as_slice(), root, &mut identify_entry) {
            let leader_path = procinfo::LocalProcessInfo::executable_path(pid)
                .map(|path| path.to_string_lossy().to_string())
                .or_else(|| {
                    table
                        .iter()
                        .find(|entry| entry.pid == pid)
                        .map(|entry| entry.name.clone())
                });
            return (leader_path, Some(agent));
        }
    }
    let leader_path = pane
        .get_foreground_process_name(policy)
        .map(super::identify::normalize_executable_path);
    let agent = leader_path.as_deref().and_then(|path| {
        let mut argv = || pane.get_foreground_process_argv(crate::pane::CachePolicy::AllowStale);
        super::proc_match::identify(path, &mut argv, &|name| {
            super::engine::manifest_id_for_alias(name)
        })
    });
    (leader_path, agent)
}

#[cfg(windows)]
pub(crate) mod facts {
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// One process-table snapshot serves every pane; herdr's number. At
    /// hundreds of panes this is what keeps identification from turning
    /// into hundreds of Toolhelp32 walks per sweep.
    const SNAPSHOT_TTL: Duration = Duration::from_millis(250);

    static SNAPSHOT: Mutex<Option<(Instant, Arc<Vec<procinfo::ProcessTableEntry>>)>> =
        Mutex::new(None);

    pub(crate) fn shared_process_table() -> Option<Arc<Vec<procinfo::ProcessTableEntry>>> {
        let mut slot = SNAPSHOT.lock();
        if let Some((at, table)) = slot.as_ref() {
            if at.elapsed() < SNAPSHOT_TTL {
                return Some(table.clone());
            }
        }
        let table = Arc::new(procinfo::process_table()?);
        *slot = Some((Instant::now(), table.clone()));
        Some(table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: u32, ppid: u32, name: &str) -> ProcessTableEntry {
        ProcessTableEntry {
            pid,
            ppid,
            name: name.to_string(),
        }
    }

    fn ident(entry: &ProcessTableEntry) -> Option<String> {
        match entry.name.as_str() {
            "claude.exe" => Some("claude".to_string()),
            "codex.exe" => Some("codex".to_string()),
            _ => None,
        }
    }

    #[test]
    fn topmost_agent_beats_its_own_children() {
        // shell(1) -> claude(2) -> node mcp(3) -> codex helper(4)
        let table = vec![
            entry(1, 0, "pwsh.exe"),
            entry(2, 1, "claude.exe"),
            entry(3, 2, "node.exe"),
            entry(4, 3, "codex.exe"),
        ];
        assert_eq!(
            select_agent(&table, 1, &mut ident),
            Some((2, "claude".to_string()))
        );
    }

    #[test]
    fn sibling_agents_are_ambiguous_and_select_nothing() {
        let table = vec![
            entry(1, 0, "pwsh.exe"),
            entry(2, 1, "claude.exe"),
            entry(3, 1, "codex.exe"),
        ];
        assert_eq!(select_agent(&table, 1, &mut ident), None);
    }

    #[test]
    fn processes_outside_the_pane_tree_do_not_leak_in() {
        let table = vec![
            entry(1, 0, "pwsh.exe"),
            entry(2, 1, "node.exe"),
            // Another pane's claude, unrelated ancestry.
            entry(9, 7, "claude.exe"),
        ];
        assert_eq!(select_agent(&table, 1, &mut ident), None);
    }

    #[test]
    fn pid_reuse_loops_terminate() {
        // 2 and 3 point at each other. The property under test is
        // termination; in a fabricated cycle both claim ancestry and the
        // walk-order-first candidate (the root) wins deterministically.
        let table = vec![
            entry(2, 3, "claude.exe"),
            entry(3, 2, "codex.exe"),
        ];
        assert_eq!(
            select_agent(&table, 2, &mut ident),
            Some((2, "claude".to_string()))
        );
    }

    #[test]
    fn the_root_itself_can_be_the_agent() {
        let table = vec![entry(5, 1, "claude.exe"), entry(6, 5, "node.exe")];
        assert_eq!(
            select_agent(&table, 5, &mut ident),
            Some((5, "claude".to_string()))
        );
    }
}
