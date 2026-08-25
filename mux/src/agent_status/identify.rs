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
    let leader_path = pane
        .get_foreground_process_name(policy)
        .map(normalize_executable_path);
    let resolved = CachedIdent {
        at: now,
        agent: leader_path.as_deref().and_then(|path| {
            // argv is fetched only if the leader turns out to be a generic
            // interpreter, and always AllowStale: the name probe one step
            // up just refreshed the leader cache, so this reuses that same
            // pid. Re-probing here could race a foreground change and pair
            // this argv with the other probe's path — and on Windows a
            // FetchImmediate would repeat a full process-table walk.
            let mut argv = || pane.get_foreground_process_argv(CachePolicy::AllowStale);
            super::proc_match::identify(path, &mut argv, &|name| {
                super::engine::manifest_id_for_alias(name)
            })
        }),
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

/// On Linux, `/proc/<pid>/exe` reads as `/path/to/bin (deleted)` once the
/// binary has been replaced on disk — routine for self-updating agents.
/// Normalizing here, at the module's single entry point for the path,
/// keeps both identification and the contract's leader-path equality
/// working across an in-place update. macOS resolves via `proc_pidpath`,
/// which never decorates the path, so this is a no-op there.
fn normalize_executable_path(path: String) -> String {
    match path.strip_suffix(" (deleted)") {
        Some(stripped) => stripped.to_string(),
        None => path,
    }
}

#[cfg(test)]
mod tests {
    // Matching-rule behavior is covered in `proc_match`'s own tests
    // against a fake alias table; this module only owns the path
    // normalization and the cache.

    #[test]
    fn replaced_binaries_lose_their_deleted_decoration() {
        assert_eq!(
            super::normalize_executable_path("/usr/local/bin/claude (deleted)".to_string()),
            "/usr/local/bin/claude"
        );
        assert_eq!(
            super::normalize_executable_path("/usr/local/bin/claude".to_string()),
            "/usr/local/bin/claude"
        );
    }
}
