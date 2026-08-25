//! pane → agent identification.
//!
//! The contract user-var is authoritative when present. Otherwise, for
//! local panes, the foreground process basename is matched against the
//! manifest id/alias table. Remote (mux-mirror) panes have no process
//! visibility, so they identify through the contract only.

use crate::pane::{CachePolicy, Pane};
use std::collections::HashMap;
use parking_lot::Mutex;
use std::time::{Duration, Instant};

/// How long a process-based identification result (positive or negative)
/// is trusted before the foreground process is consulted again.
const IDENT_TTL: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct CachedIdent {
    at: Instant,
    /// The tty's foreground process-group-leader path as last probed.
    leader_path: Option<String>,
    /// The manifest id that path resolves to, if any.
    agent: Option<String>,
}

static IDENT_CACHE: Mutex<Option<HashMap<crate::pane::PaneId, CachedIdent>>> = Mutex::new(None);

/// One probe serves both questions about a pane's foreground leader; the
/// result is cached for [`IDENT_TTL`]. The lock is never held across the
/// process probe, which can block on the process table.
fn probe(pane: &dyn Pane) -> CachedIdent {
    let pane_id = pane.pane_id();
    let now = Instant::now();

    let seeded = {
        let mut guard = IDENT_CACHE.lock();
        let cache = guard.get_or_insert_with(HashMap::new);
        match cache.get(&pane_id) {
            Some(cached) if now.duration_since(cached.at) < IDENT_TTL => {
                return cached.clone();
            }
            Some(_) => true,
            None => false,
        }
    };

    // The first probe per pane forces a fetch: on a headless mux server
    // nothing else ever warms the process-info cache, so AllowStale would
    // answer None forever. Once seeded, AllowStale suffices — its stale
    // path refreshes on a worker thread instead of blocking this one.
    let policy = if seeded {
        CachePolicy::AllowStale
    } else {
        CachePolicy::FetchImmediate
    };
    let leader_path = pane.get_foreground_process_name(policy);
    let resolved = CachedIdent {
        at: now,
        agent: leader_path
            .as_deref()
            .and_then(identify_from_process_path),
        leader_path,
    };

    let mut guard = IDENT_CACHE.lock();
    let cache = guard.get_or_insert_with(HashMap::new);
    cache.insert(pane_id, resolved.clone());
    // Opportunistic bound rather than tracking pane lifetimes. Never
    // clear wholesale: on a server with thousands of panes that would
    // wipe every live entry each sweep and degrade every probe to a
    // blocking FetchImmediate on the main thread. Expired entries go
    // first; if the cache is still over the cap, that many panes are
    // genuinely live, so shed the oldest half and keep the hot rest.
    const CACHE_CAP: usize = 4096;
    if cache.len() > CACHE_CAP {
        cache.retain(|_, cached| now.duration_since(cached.at) < IDENT_TTL);
    }
    if cache.len() > CACHE_CAP {
        let mut ages: Vec<Instant> = cache.values().map(|cached| cached.at).collect();
        ages.sort_unstable();
        let cutoff = ages[ages.len() / 2];
        cache.retain(|_, cached| cached.at >= cutoff);
    }
    resolved
}

/// Identify which agent manifest applies to this pane based on its
/// foreground process. Returns the manifest id.
pub fn identify_by_process(pane: &dyn Pane) -> Option<String> {
    if pane.is_remote_mirror() {
        return None;
    }
    probe(pane).agent
}

/// The pane's foreground process-group-leader path. The leader stays the
/// agent while it runs tool subprocesses and reverts to the shell when
/// the agent exits, which makes "the contract's emitter is gone"
/// detectable without guessing at a re-report cadence.
pub fn leader_path(pane: &dyn Pane) -> Option<String> {
    if pane.is_remote_mirror() {
        return None;
    }
    probe(pane).leader_path
}

pub fn forget_pane(pane_id: crate::pane::PaneId) {
    if let Some(cache) = IDENT_CACHE.lock().as_mut() {
        cache.remove(&pane_id);
    }
}

/// Match a foreground executable path to a manifest id: by basename, and
/// failing that by the last few directory components. Version-managed
/// launchers exec a binary named after the version — e.g. Claude Code runs
/// as `~/.local/share/claude/versions/2.1.239` — so the agent's name only
/// appears as a parent directory.
fn identify_from_process_path(path: &str) -> Option<String> {
    let name = basename(path).to_lowercase();
    if let Some(id) = super::engine::manifest_id_for_alias(&name) {
        return Some(id);
    }
    // Directory components only vouch for a *versioned launcher* layout
    // (`~/.local/share/claude/versions/2.1.239`). Without the version-like
    // basename gate, any executable under an alias-named directory would
    // be misidentified — e.g. everything in `/home/pi/.local/bin`.
    if !looks_like_version(&name) {
        return None;
    }
    path.rsplit(['/', '\\'])
        .skip(1)
        .take(2)
        .find_map(|component| super::engine::manifest_id_for_alias(&component.to_lowercase()))
}

/// A launcher-style version basename: starts with a digit, rest is
/// digits/letters/dots/dashes ("2.1.239", "1.0.0-rc1").
fn looks_like_version(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

fn basename(path: &str) -> &str {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.strip_suffix(".exe").unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::basename;

    #[test]
    fn basenames() {
        assert_eq!(basename("/usr/local/bin/claude"), "claude");
        assert_eq!(basename("codex"), "codex");
        assert_eq!(basename("C:\\tools\\codex.exe"), "codex");
    }

    #[test]
    fn versioned_launcher_paths_identify_by_parent_directory() {
        assert_eq!(
            super::identify_from_process_path("/Users/x/.local/share/claude/versions/2.1.239")
                .as_deref(),
            Some("claude")
        );
        assert_eq!(
            super::identify_from_process_path("/usr/local/bin/claude").as_deref(),
            Some("claude")
        );
        assert_eq!(super::identify_from_process_path("/bin/zsh"), None);
        // The component match only looks at the trailing directories, so a
        // deep unrelated prefix cannot misidentify.
        assert_eq!(
            super::identify_from_process_path("/home/claude/projects/tool/bin/node"),
            None
        );
    }

    /// Directory names vouch only for versioned launchers: an ordinary
    /// binary living under an alias-named directory (a user named `pi`,
    /// say) must not be misidentified.
    #[test]
    fn alias_named_directories_do_not_claim_ordinary_binaries() {
        assert_eq!(
            super::identify_from_process_path("/home/pi/.local/bin/htop"),
            None
        );
        // Even a version-like basename doesn't help when the alias is a
        // grandparent past the two-component window — `pi` here is the
        // user's home, not a launcher directory.
        assert_eq!(
            super::identify_from_process_path("/home/pi/.local/bin/2.0.1"),
            None
        );
        // A real launcher layout for the same agent still identifies.
        assert_eq!(
            super::identify_from_process_path("/home/x/pi/versions/2.0.1").as_deref(),
            Some("pi")
        );
        // The gate also bounds the walk to the last two directories.
        assert_eq!(
            super::identify_from_process_path("/opt/claude/deep/nested/2.1.0"),
            None
        );
    }
}
