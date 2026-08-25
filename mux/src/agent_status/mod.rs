//! Agent status detection core.
//!
//! Classifies panes that run coding agents (Claude Code, Codex, …) as
//! Working / Blocked / Idle by arbitrating three signal layers:
//!
//! 1. the `THINKTERM_AGENT` user-var contract ([`contract`]) — the agent
//!    (or its hook script) reports identity, session, and state;
//! 2. manifest-driven screen rules ([`engine`]) — the pane's live screen
//!    tail plus OSC title/progress, evaluated against herdr-compatible
//!    rule files. A matching rule outranks the contract's *state*, because
//!    the two known contract gaps (Esc interrupt, permission-prompt
//!    cancel) both look like "contract says working, screen shows idle";
//! 3. nothing: when neither speaks, a known agent settles to Idle (never
//!    Blocked).
//!
//! Detection runs in the process that **owns** the pane: the predicate is
//! `!is_remote_mirror() && !is_dead()`. A mux server therefore classifies
//! all of its panes (full screen-rule coverage for remote clients), the
//! GUI's embedded mux classifies its local panes, and a ClientPane answers
//! [`Pane::agent_status`] from whatever the owning server pushed. That one
//! predicate is the whole no-double-detection story.

pub mod contract;
pub mod engine;
mod hysteresis;
pub mod identify;
mod proc_match;

use crate::pane::{Pane, PaneId};
use crate::{Mux, MuxNotification};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thinkterm_proto::{AgentEvidence, AgentState, AgentStatus};

/// Minimum interval between screen reads for one pane even while output
/// is flowing.
const SCREEN_READ_FLOOR: Duration = Duration::from_millis(150);
/// How many rows from the bottom of the live screen the rules may see.
/// Large enough to cover a whole typical viewport: manifests address the
/// top of the screen too (`top_non_empty_lines`), and a prompt drawn up
/// there is invisible to every rule if the slice stops short of it.
const TAIL_ROWS: isize = 64;
/// At most one evaluation pass per this interval, process-wide. A leading
/// edge throttle rather than a trailing debounce: an agent that prints
/// continuously must still be classified while it streams.
const EVAL_INTERVAL: Duration = Duration::from_millis(150);
/// Quiet panes emit no PaneOutput, but contract freshness expiry, agents
/// exiting back to a shell, and pane death still need a pulse.
const SAFETY_TICK: Duration = Duration::from_secs(2);

struct ScreenCache {
    seqno: termwiz::surface::SequenceNo,
    title: String,
    progress: String,
    at: Instant,
    /// Manifest generation the verdict was computed under; a rules reload
    /// invalidates it even if this cache was written after the reload's
    /// sweep by a racing evaluation.
    generation: u64,
    verdict: CachedVerdict,
}

#[derive(Clone, Copy)]
enum CachedVerdict {
    State(AgentState),
    Freeze,
    NoMatch,
}

struct AgentPaneRecord {
    agent_id: String,
    session_id: Option<String>,
    state: AgentState,
    evidence: AgentEvidence,
    since_unix: u64,
    ended: bool,
    screen_cache: Option<ScreenCache>,
    /// What consumers currently see; publication notifies only on change.
    published: Option<AgentStatus>,
    /// Confirmation window for guarded transitions; see [`hysteresis`].
    pending: hysteresis::Pending,
    /// The raw contract string last seen, and the foreground leader
    /// observed when it changed: together they prove the emitter is still
    /// alive when the contract itself has gone stale.
    contract_raw: Option<String>,
    contract_leader: Option<String>,
}

lazy_static::lazy_static! {
    static ref REGISTRY: RwLock<HashMap<PaneId, AgentPaneRecord>> = RwLock::new(HashMap::new());
    static ref PENDING: Mutex<HashSet<PaneId>> = Mutex::new(HashSet::new());
    static ref PROCESS_PREFERENCE: RwLock<Option<Box<dyn Fn() -> bool + Send + Sync>>> =
        RwLock::new(None);
}
static TICK_SCHEDULED: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// Cached gate consulted on every notification: the full computation
/// (config lookup + preference closure) is too heavy for the pty parser
/// threads' hot path, so it runs only on the refresh points (init, the
/// safety tick, config reload, the settings toggle).
static ENABLED: AtomicBool = AtomicBool::new(false);

/// The status the owning process last published for this pane, if any.
/// This is what `Pane::agent_status`'s default body answers with.
pub fn status_for_pane(pane_id: PaneId) -> Option<AgentStatus> {
    REGISTRY
        .read()
        .get(&pane_id)
        .and_then(|record| record.published.clone())
}

/// Install a process-local veto consulted alongside the Lua config option.
/// The GUI wires its native-settings toggle through this so the user-facing
/// switch governs the GUI's own detector; headless servers install nothing
/// and follow `agent_status_detection` alone.
pub fn set_process_preference(preference: impl Fn() -> bool + Send + Sync + 'static) {
    *PROCESS_PREFERENCE.write() = Some(Box::new(preference));
    refresh_enabled();
}

fn compute_enabled() -> bool {
    if !config::configuration().agent_status_detection {
        return false;
    }
    PROCESS_PREFERENCE.read().as_ref().map(|f| f()).unwrap_or(true)
}

/// Re-evaluate the gate. Call after anything it depends on changes: the
/// GUI's settings toggle does, config reload and the safety tick do.
pub fn refresh_enabled() {
    ENABLED.store(compute_enabled(), Ordering::Release);
}

pub fn detection_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Install the process-wide detector: one mux subscription plus the safety
/// tick. Idempotent; the GUI and standalone server call this only after their
/// main-thread scheduler is ready. CLI proxy and TUI processes never call it.
pub fn initialize_mux(mux: &Mux) {
    // Keep a future call-site regression from taking down the whole app. Do
    // not claim INSTALLED here: the owner can retry once its scheduler exists.
    if !promise::spawn::is_scheduler_configured() {
        log::error!("agent status detector initialization deferred: no scheduler is configured");
        return;
    }
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    refresh_enabled();
    std::mem::forget(config::subscribe_to_config_reload(|| {
        // Deferred, never inline: this callback runs while the config
        // reload still holds the global config mutex, and refresh_enabled
        // -> config::configuration() would re-lock it — a same-thread
        // deadlock that freezes the whole process. Every config
        // subscriber in the tree defers for exactly this reason.
        promise::spawn::spawn_into_main_thread(async {
            refresh_enabled();
        })
        .detach();
        true
    }));
    // Load the manifests now, off the evaluation path: the first
    // evaluation must not do filesystem IO on the main thread.
    engine::warm();
    mux.subscribe(|notification| {
        // The flag-off promise is byte-identical behavior: one atomic
        // read per notification and nothing else.
        if !detection_enabled() {
            return true;
        }
        match notification {
            MuxNotification::PaneOutput(pane_id) | MuxNotification::PaneAdded(pane_id) => {
                mark_dirty(pane_id)
            }
            MuxNotification::Alert { pane_id, alert } if alert_is_interesting(&alert) => {
                mark_dirty(pane_id)
            }
            // Evict only, never publish: this closure runs inside
            // Mux::notify, and clients already learn of the pane's demise
            // from PaneRemoved itself — a trailing AgentStatusChanged for a
            // pane they just unmapped would only trigger wasted resyncs.
            MuxNotification::PaneRemoved(pane_id) => {
                evict_pane(pane_id);
            }
            _ => {}
        }
        true
    });
    promise::spawn::spawn_into_main_thread(async {
        loop {
            smol::Timer::after(SAFETY_TICK).await;
            // This loop is the only pulse quiet panes get; a panic below
            // (including inside the config read) must re-arm the tick, not
            // silently end detection for the remaining process lifetime.
            let tick = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                refresh_enabled();
                if detection_enabled() {
                    let Some(mux) = Mux::try_get() else {
                        return;
                    };
                    for pane in mux.iter_panes() {
                        mark_dirty(pane.pane_id());
                    }
                    // Also pulse registry entries whose pane the mux no
                    // longer knows: a PaneRemoved eviction that raced an
                    // in-flight evaluation can resurrect a record, and
                    // the drain's get_pane miss is what cleans it up.
                    let ghosts: Vec<PaneId> = REGISTRY.read().keys().copied().collect();
                    for pane_id in ghosts {
                        mark_dirty(pane_id);
                    }
                } else {
                    // Quiet panes produce no output to trigger the
                    // enabled→disabled sweep; run it from here so remote
                    // clients are not left frozen on stale statuses.
                    drain_and_evaluate();
                }
            }));
            if tick.is_err() {
                log::error!("agent status safety tick panicked; detection continues");
            }
        }
    })
    .detach();
}

fn alert_is_interesting(alert: &wezterm_term::Alert) -> bool {
    use wezterm_term::Alert;
    matches!(
        alert,
        Alert::SetUserVar { .. }
            | Alert::Progress(_)
            | Alert::WindowTitleChanged(_)
            | Alert::TabTitleChanged(_)
            | Alert::IconTitleChanged(_)
    )
}

fn mark_dirty(pane_id: PaneId) {
    if !detection_enabled() {
        return;
    }
    PENDING.lock().insert(pane_id);
    if TICK_SCHEDULED.swap(true, Ordering::AcqRel) {
        return;
    }
    // spawn_into_main_thread, not spawn: PaneOutput notifications arrive on
    // the pty parser threads, where no local scheduler exists. The timer
    // then runs on the main executor alongside everything else.
    promise::spawn::spawn_into_main_thread(async {
        smol::Timer::after(EVAL_INTERVAL).await;
        TICK_SCHEDULED.store(false, Ordering::Release);
        // The flag is already reset, so a panic here self-heals on the next
        // mark_dirty; catch it anyway so the executor task dies quietly and
        // the failure is visible in the log.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(drain_and_evaluate)).is_err() {
            log::error!("agent status evaluation panicked; detection continues");
        }
    })
    .detach();
}

/// Reload the screen-rule manifests and re-judge every known pane. The
/// engine swap alone is not enough: a pane whose screen has not changed
/// since its last evaluation keeps serving a cached verdict (the
/// seqno-equality reuse), which was computed under the *old* rules and
/// would survive indefinitely. Safe to call from any thread; the caller
/// may run it on a worker since loading reads manifest files. Returns how
/// many manifest files were rejected, so the caller can report a partial
/// reload instead of unconditional success.
pub fn reload_rules() -> usize {
    let rejected = engine::reload_manifests();
    let panes: Vec<PaneId> = {
        let mut registry = REGISTRY.write();
        for record in registry.values_mut() {
            record.screen_cache = None;
        }
        registry.keys().copied().collect()
    };
    for pane_id in panes {
        mark_dirty(pane_id);
    }
    rejected
}

/// Drop a pane's state without telling anyone. Returns whether a published
/// status was dropped so callers outside the notify path can decide to
/// publish the eviction. Deliberately never notifies: the PaneRemoved
/// subscriber runs inside `Mux::notify`, where a synchronous nested notify
/// re-enters the subscriber lock and deadlocks the process.
///
/// Invariant note: every other mutation of a published status enqueues a
/// trailing AgentStatusChanged, which is what lets the client's
/// authoritative snapshot fetch converge against live pushes. This
/// silent path is safe only because the PaneRemoved PDU itself follows
/// on the same dispatch channel; do not add further silent mutations.
fn evict_pane(pane_id: PaneId) -> bool {
    identify::forget_pane(pane_id);
    REGISTRY
        .write()
        .remove(&pane_id)
        .and_then(|record| record.published)
        .is_some()
}

/// Notify consumers that a pane's status changed. Always deferred to a
/// fresh main-thread task, never synchronous: publish sites are reachable
/// from inside Mux subscriber callbacks, and a synchronous notify there
/// re-enters the subscriber lock (see `evict_pane`). Unit tests have no
/// scheduler and record into a log instead.
fn publish_change(pane_id: PaneId) {
    #[cfg(test)]
    {
        tests::record_publish(pane_id);
    }
    #[cfg(not(test))]
    promise::spawn::spawn_into_main_thread(async move {
        if let Some(mux) = Mux::try_get() {
            mux.notify(MuxNotification::AgentStatusChanged(pane_id));
        }
    })
    .detach();
}

fn drain_and_evaluate() {
    if !detection_enabled() {
        // Enabled→disabled edge: evict everything, loudly.
        let ids: Vec<PaneId> = REGISTRY.read().keys().copied().collect();
        for pane_id in ids {
            if evict_pane(pane_id) {
                publish_change(pane_id);
            }
        }
        PENDING.lock().clear();
        return;
    }
    let dirty: Vec<PaneId> = PENDING.lock().drain().collect();
    let Some(mux) = Mux::try_get() else {
        return;
    };
    for pane_id in dirty {
        let Some(pane) = mux.get_pane(pane_id) else {
            if evict_pane(pane_id) {
                publish_change(pane_id);
            }
            continue;
        };
        if pane.is_remote_mirror() {
            continue;
        }
        if pane.is_dead() {
            if evict_pane(pane_id) {
                publish_change(pane_id);
            }
            continue;
        }
        if evaluate_pane(pane.as_ref()) {
            publish_change(pane_id);
        }
    }
    // Panes mid-confirmation must not stall until the next output or the
    // 2s safety tick: re-mark them so the next 150ms round re-checks.
    let confirming: Vec<PaneId> = REGISTRY
        .read()
        .iter()
        .filter(|(_, record)| record.pending.is_open())
        .map(|(id, _)| *id)
        .collect();
    for pane_id in confirming {
        mark_dirty(pane_id);
    }
}

/// Classify one pane and update the registry. Returns whether the
/// published status changed.
fn evaluate_pane(pane: &dyn Pane) -> bool {
    let pane_id = pane.pane_id();

    let user_vars = pane.copy_user_vars();
    let contract_raw = user_vars.get(contract::USER_VAR_NAME).cloned();
    let parsed_contract = contract_raw.as_deref().and_then(contract::parse);

    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Identity, in strict priority order:
    //   R1: a live foreground process matching a manifest — ground truth
    //       for what is running *now*; a user var is a message from the
    //       past and never overrides it.
    //   R2: otherwise, a contract that has not declared `ended` — either
    //       still fresh, or (for long-silent native emitters) proven live
    //       because the pane's foreground leader is unchanged since the
    //       contract was last emitted.
    //   R3: otherwise no agent: the record is evicted, so a pane that ran
    //       claude once stops being classified the moment claude is gone,
    //       and `cat` output can no longer impersonate a permission
    //       prompt.
    let process_agent = identify::identify_by_process(pane);
    let process_backed = process_agent.is_some();
    let leader = identify::leader_path(pane);
    let contract_identity = parsed_contract.as_ref().and_then(|c| {
        if c.ended || process_agent.is_some() {
            return None;
        }
        match REGISTRY.read().get(&pane_id) {
            // A known pane must still have the emitter as its foreground
            // leader — freshness alone is not liveness. An idle contract
            // stays fresh forever and a working one for hours; without
            // this check an agent that died without saying `ended=1`
            // would pin its identity onto the shell. Only a definite
            // mismatch (both sides observed, different) kills the
            // identity: a transient probe failure must not evict.
            Some(record) if record.agent_id == c.agent_id => {
                let emitter_gone = matches!(
                    (&record.contract_leader, &leader),
                    (Some(recorded), Some(current)) if recorded != current
                );
                if emitter_gone {
                    return None;
                }
            }
            // No history for this agent to validate against: only a
            // fresh contract may establish identity, so a relic var
            // cannot resurrect.
            _ => {
                if !contract::state_is_fresh(c, now_unix) {
                    return None;
                }
            }
        }
        Some(c.agent_id.clone())
    });
    let agent_id = process_agent.or(contract_identity);

    let Some(agent_id) = agent_id else {
        let removed = REGISTRY.write().remove(&pane_id);
        let had_record = removed.is_some();
        let had_published = removed.and_then(|record| record.published).is_some();
        if had_record {
            // An agent just left: drop its identity cache entry so the
            // next occupant is probed fresh, and clear its retained OSC
            // signals so they cannot leak into whatever runs next. For a
            // pane that never was an agent, the negative identification
            // stays cached for its TTL — dropping it here would force a
            // synchronous process probe per shell pane per round.
            identify::forget_pane(pane_id);
            pane.clear_agent_osc_evidence();
        }
        return had_published;
    };

    // Real emissions only: `get_title`/`get_progress` substitute display
    // defaults (process basename, cleared progress) that would make the
    // manifests' idle rules match unconditionally.
    let osc = pane.agent_osc_evidence();
    let mut title = osc.title.unwrap_or_default();
    let mut progress = osc.progress.unwrap_or_default();
    let now = Instant::now();

    // Phase 1, read lock only: occupant turnover and verdict reuse. The
    // write lock must not be held across the screen read and the manifest
    // scan — `Pane::agent_status` is answered from dispatch and session
    // handlers that must not stall behind another pane's evaluation.
    // Read the seqno before taking the registry lock: get_current_seqno
    // takes the terminal lock, and holding registry-then-terminal here
    // would be a lock-order trap for any future terminal-then-registry
    // path.
    let current_seqno = pane.get_current_seqno();
    let (turnover, reused_verdict) = {
        let registry = REGISTRY.read();
        let record = registry.get(&pane_id);
        // A different agent or session replaces the record wholesale so
        // nothing from the previous occupant carries over — including its
        // retained OSC evidence, which the terminal still holds and which
        // must not classify the new occupant for even one round.
        let turnover = record
            .map(|record| {
                record.agent_id != agent_id
                    || parsed_contract
                        .as_ref()
                        .map(|c| {
                            // Only a contract describing *this* agent can
                            // declare a session change; a relic var from a
                            // previous occupant must not tear the record
                            // down every round (which would also destroy
                            // the hysteresis window and the OSC evidence).
                            c.agent_id == record.agent_id
                                && c.session_id.is_some()
                                && record.session_id != c.session_id
                        })
                        .unwrap_or(false)
            })
            .unwrap_or(false);
        let reused_verdict = if turnover {
            None
        } else {
            record.and_then(|record| {
                // While a guarded transition is being confirmed, the
                // age-based reuse clause is suspended: counting a cached
                // verdict as a confirmation would confirm against a stale
                // observation and halve the effective debounce. Reuse on
                // an unchanged seqno stays — an unmoved screen genuinely
                // is evidence of stability.
                let confirming = record.pending.is_open();
                record
                    .screen_cache
                    .as_ref()
                    .filter(|c| {
                        c.generation == engine::manifest_generation()
                            && c.title == title
                            && c.progress == progress
                            && (c.seqno == current_seqno
                                || (!confirming
                                    && now.duration_since(c.at) < SCREEN_READ_FLOOR))
                    })
                    .map(|c| c.verdict)
            })
        };
        (turnover, reused_verdict)
    };

    if turnover {
        REGISTRY.write().remove(&pane_id);
        pane.clear_agent_osc_evidence();
        title.clear();
        progress.clear();
    }

    // Phase 2, no lock: the screen read and the manifest scan.
    let (verdict, fresh_cache) = match reused_verdict {
        Some(verdict) => (verdict, None),
        None => {
            let screen = read_screen_tail(pane);
            let verdict = match engine::evaluate(
                &agent_id,
                &engine::DetectionInput {
                    screen: &screen,
                    osc_title: &title,
                    osc_progress: &progress,
                },
            ) {
                engine::ScreenVerdict::State(state) => CachedVerdict::State(state),
                engine::ScreenVerdict::Freeze => CachedVerdict::Freeze,
                engine::ScreenVerdict::NoMatch => CachedVerdict::NoMatch,
            };
            // Stamped with the seqno captured *before* the read: output
            // landing during evaluation must invalidate this verdict, and
            // a pane that then blocks on input never advances the seqno
            // again, so a post-read stamp would pin the stale verdict for
            // good.
            let cache = ScreenCache {
                seqno: current_seqno,
                title: title.clone(),
                progress: progress.clone(),
                at: now,
                generation: engine::manifest_generation(),
                verdict,
            };
            (verdict, Some(cache))
        }
    };

    let contract_state = parsed_contract.as_ref().filter(|c| c.agent_id == agent_id).and_then(|c| {
        if c.ended {
            Some(AgentState::Idle)
        } else if c.state != AgentState::Unknown && contract::state_is_fresh(c, now_unix) {
            Some(c.state)
        } else {
            None
        }
    });

    // Phase 3, write lock: apply. The registry may have changed while
    // unlocked (a PaneRemoved eviction from another thread); re-fetching
    // here keeps the apply self-consistent. A lost race can re-create a
    // record for a removed pane; the safety tick pulses every registry id,
    // so the next drain's get_pane miss evicts it.
    let mut registry = REGISTRY.write();
    let registry = &mut *registry;
    // Before the placeholder insert below: a brand-new pane must present
    // as "no previous state", not as the placeholder's Unknown.
    let previous_state = registry.get(&pane_id).map(|r| r.state);
    if let Some(cache) = fresh_cache {
        if let Some(record) = registry.get_mut(&pane_id) {
            record.screen_cache = Some(cache);
        } else {
            registry.insert(
                pane_id,
                AgentPaneRecord {
                    agent_id: agent_id.clone(),
                    session_id: None,
                    state: AgentState::Unknown,
                    evidence: AgentEvidence::Fallback,
                    since_unix: now_unix,
                    ended: false,
                    screen_cache: Some(cache),
                    published: None,
                    pending: Default::default(),
                    contract_raw: None,
                    contract_leader: None,
                },
            );
        }
    }

    let (state, evidence) = match verdict {
        CachedVerdict::State(state) => (state, AgentEvidence::Screen),
        CachedVerdict::Freeze => match previous_state {
            Some(state) => (state, AgentEvidence::Screen),
            None => match contract_state {
                Some(state) => (state, AgentEvidence::Contract),
                None => (AgentState::Idle, AgentEvidence::Fallback),
            },
        },
        CachedVerdict::NoMatch => match contract_state {
            Some(state) => (state, AgentEvidence::Contract),
            None => (AgentState::Idle, AgentEvidence::Fallback),
        },
    };

    let record = registry.entry(pane_id).or_insert_with(|| AgentPaneRecord {
        agent_id: agent_id.clone(),
        session_id: None,
        state,
        evidence,
        since_unix: now_unix,
        ended: false,
        screen_cache: None,
        published: None,
        pending: Default::default(),
        contract_raw: None,
        contract_leader: None,
    });
    record.agent_id = agent_id;
    if record.contract_raw != contract_raw {
        // A (re-)emission: snapshot who the foreground leader was, so a
        // later staleness check can tell "silent but alive" from "gone".
        record.contract_raw = contract_raw;
        record.contract_leader = leader;
    }
    match parsed_contract.as_ref() {
        // Contract fields describe *this* agent only; a relic var from a
        // previous occupant must not decorate the new one.
        Some(c) if c.agent_id == record.agent_id => {
            if c.session_id.is_some() {
                record.session_id = c.session_id.clone();
            }
            // A live process outranks a claimed ending: `ended` is a
            // farewell, and the process still being foreground means it
            // was premature or belongs to a finished sub-session.
            record.ended = c.ended && !process_backed;
        }
        _ => {}
    }
    // Guarded transitions keep publishing the held state until the raw
    // observation proves stable; the first classification is never held.
    let (state, evidence) = if record.published.is_some()
        && record.pending.should_hold(record.state, state, evidence, now)
    {
        (record.state, record.evidence)
    } else {
        (state, evidence)
    };
    if record.state != state {
        record.state = state;
        record.since_unix = now_unix;
    }
    record.evidence = evidence;

    let status = AgentStatus {
        agent_id: record.agent_id.clone(),
        state: record.state,
        evidence: record.evidence,
        session_id: record.session_id.clone(),
        since_unix: record.since_unix,
        ended: record.ended,
    };
    if record.published.as_ref() == Some(&status) {
        false
    } else {
        record.published = Some(status);
        true
    }
}

/// The pane's live screen bottom, independent of the user's scroll
/// position: scrollback sits below `physical_top`, so slicing from the
/// physical screen ignores the viewport.
fn read_screen_tail(pane: &dyn Pane) -> String {
    let dims = pane.get_dimensions();
    if dims.cols == 0 || dims.viewport_rows == 0 {
        return String::new();
    }
    let last = dims.physical_top + dims.viewport_rows as isize;
    let start = (last - TAIL_ROWS).max(dims.physical_top);
    let (_first, lines) = pane.get_lines(start..last);
    let mut text = String::new();
    for line in &lines {
        text.push_str(&line.as_str());
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::{CachePolicy, ForEachPaneLogicalLine, LogicalLine, WithPaneLines};
    use crate::renderable::*;
    use crate::DomainId;
    use parking_lot::MappedMutexGuard;
    use rangeset::RangeSet;
    use std::ops::Range;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use termwiz::surface::{Line, SequenceNo};
    use url::Url;
    use wezterm_term::color::ColorPalette;
    use wezterm_term::{KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalSize};

    static NEXT_PANE_ID: AtomicUsize = AtomicUsize::new(9000);

    lazy_static::lazy_static! {
        static ref PUBLISH_LOG: parking_lot::Mutex<Vec<PaneId>> =
            parking_lot::Mutex::new(Vec::new());
    }

    /// Test sink for `publish_change`: no scheduler exists under
    /// `cargo test`, so publications are recorded instead of notified.
    pub(super) fn record_publish(pane_id: PaneId) {
        PUBLISH_LOG.lock().push(pane_id);
    }

    /// Tests run in parallel and share the log; filter by the caller's
    /// unique pane id.
    fn publishes_for(pane_id: PaneId) -> usize {
        PUBLISH_LOG.lock().iter().filter(|id| **id == pane_id).count()
    }

    /// One dirty-pane step exactly as `drain_and_evaluate` performs it.
    fn drain_step(pane: &dyn Pane) {
        if evaluate_pane(pane) {
            publish_change(pane.pane_id());
        }
    }

    /// Sequence tests run in microseconds, so the `SCREEN_READ_FLOOR`
    /// reuse clause would serve every frame from cache and mask the very
    /// transitions under test. Age the cache as a real 150ms round gap
    /// would, then step.
    fn drain_step_next_round(pane: &dyn Pane) {
        if let Some(cache) = REGISTRY
            .write()
            .get_mut(&pane.pane_id())
            .and_then(|record| record.screen_cache.as_mut())
        {
            if let Some(aged) = cache.at.checked_sub(SCREEN_READ_FLOOR * 2) {
                cache.at = aged;
            }
        }
        drain_step(pane);
    }

    struct FakeAgentPane {
        id: PaneId,
        screen: parking_lot::Mutex<String>,
        title: parking_lot::Mutex<String>,
        osc_evidence: parking_lot::Mutex<wezterm_term::AgentOscEvidence>,
        user_vars: parking_lot::Mutex<HashMap<String, String>>,
        seqno: AtomicUsize,
        process: parking_lot::Mutex<Option<String>>,
        argv: parking_lot::Mutex<Option<Vec<String>>>,
        argv_probes: AtomicUsize,
        probes: AtomicUsize,
        remote_mirror: bool,
        dead: AtomicBool,
        /// When set, the next `get_lines` returns the current screen and
        /// then applies this content and bumps the seqno -- emulating
        /// output that lands while an evaluation is still running.
        swap_after_read: parking_lot::Mutex<Option<String>>,
    }

    impl FakeAgentPane {
        fn new(process: Option<&str>) -> Arc<Self> {
            Arc::new(Self {
                id: NEXT_PANE_ID.fetch_add(1, Ordering::SeqCst),
                screen: parking_lot::Mutex::new(String::new()),
                title: parking_lot::Mutex::new(String::new()),
                osc_evidence: parking_lot::Mutex::new(Default::default()),
                user_vars: parking_lot::Mutex::new(HashMap::new()),
                seqno: AtomicUsize::new(1),
                process: parking_lot::Mutex::new(process.map(|p| p.to_string())),
                argv: parking_lot::Mutex::new(None),
                argv_probes: AtomicUsize::new(0),
                probes: AtomicUsize::new(0),
                remote_mirror: false,
                dead: AtomicBool::new(false),
                swap_after_read: parking_lot::Mutex::new(None),
            })
        }

        fn set_screen(&self, text: &str) {
            *self.screen.lock() = text.to_string();
            self.seqno.fetch_add(1, Ordering::SeqCst);
        }

        fn set_contract(&self, value: &str) {
            self.user_vars
                .lock()
                .insert(contract::USER_VAR_NAME.to_string(), value.to_string());
            self.seqno.fetch_add(1, Ordering::SeqCst);
        }

        fn clear_contract(&self) {
            self.user_vars.lock().remove(contract::USER_VAR_NAME);
            self.seqno.fetch_add(1, Ordering::SeqCst);
        }

        fn set_osc_title(&self, title: &str) {
            self.osc_evidence.lock().title = Some(title.to_string());
            self.seqno.fetch_add(1, Ordering::SeqCst);
        }

        /// Arrange for `text` to land on the screen immediately after the
        /// next `get_lines`, as terminal output arriving during an
        /// evaluation does.
        fn set_screen_after_next_read(&self, text: &str) {
            *self.swap_after_read.lock() = Some(text.to_string());
        }

        /// Change the foreground process and drop the identity cache's
        /// memory of this pane, as the 5s TTL would in real time.
        fn set_process(&self, process: Option<&str>) {
            *self.process.lock() = process.map(|p| p.to_string());
            identify::forget_pane(self.id);
            self.seqno.fetch_add(1, Ordering::SeqCst);
        }

        /// The leader's argv, as identification sees it when the leader
        /// executable is a generic interpreter.
        fn set_argv(&self, argv: &[&str]) {
            *self.argv.lock() = Some(argv.iter().map(|a| a.to_string()).collect());
        }
    }

    impl Pane for FakeAgentPane {
        fn pane_id(&self) -> PaneId {
            self.id
        }
        fn get_cursor_position(&self) -> StableCursorPosition {
            StableCursorPosition::default()
        }
        fn get_current_seqno(&self) -> SequenceNo {
            self.seqno.load(Ordering::SeqCst)
        }
        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            _: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            unimplemented!();
        }
        fn with_lines_mut(
            &self,
            _stable_range: Range<StableRowIndex>,
            _with_lines: &mut dyn WithPaneLines,
        ) {
            unimplemented!();
        }
        fn for_each_logical_line_in_stable_range_mut(
            &self,
            _lines: Range<StableRowIndex>,
            _for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
            unimplemented!();
        }
        fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            let all: Vec<Line> = self
                .screen
                .lock()
                .lines()
                .map(|l| Line::from_text(l, &Default::default(), 1, None))
                .collect();
            let start = lines.start.max(0) as usize;
            let out: Vec<Line> = all
                .into_iter()
                .skip(start)
                .take((lines.end - lines.start).max(0) as usize)
                .collect();
            if let Some(next) = self.swap_after_read.lock().take() {
                *self.screen.lock() = next;
                self.seqno.fetch_add(1, Ordering::SeqCst);
            }
            (lines.start, out)
        }
        fn get_logical_lines(&self, _lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            unimplemented!();
        }
        fn get_dimensions(&self) -> RenderableDimensions {
            let rows = self.screen.lock().lines().count().max(1);
            RenderableDimensions {
                cols: 120,
                viewport_rows: rows,
                scrollback_rows: rows,
                physical_top: 0,
                scrollback_top: 0,
                ..Default::default()
            }
        }
        fn get_title(&self) -> String {
            self.title.lock().clone()
        }
        fn agent_osc_evidence(&self) -> wezterm_term::AgentOscEvidence {
            self.osc_evidence.lock().clone()
        }
        fn clear_agent_osc_evidence(&self) {
            *self.osc_evidence.lock() = Default::default();
        }
        fn copy_user_vars(&self) -> HashMap<String, String> {
            self.user_vars.lock().clone()
        }
        fn get_foreground_process_name(&self, _policy: CachePolicy) -> Option<String> {
            self.probes.fetch_add(1, Ordering::SeqCst);
            self.process.lock().clone()
        }
        fn get_foreground_process_argv(&self, _policy: CachePolicy) -> Option<Vec<String>> {
            self.argv_probes.fetch_add(1, Ordering::SeqCst);
            self.argv.lock().clone()
        }
        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            unimplemented!();
        }
        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            unimplemented!();
        }
        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            unimplemented!();
        }
        fn resize(&self, _size: TerminalSize) -> anyhow::Result<()> {
            unimplemented!();
        }
        fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            unimplemented!();
        }
        fn key_up(&self, _: KeyCode, _: KeyModifiers) -> anyhow::Result<()> {
            unimplemented!();
        }
        fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
            unimplemented!();
        }
        fn is_dead(&self) -> bool {
            self.dead.load(Ordering::SeqCst)
        }
        fn palette(&self) -> ColorPalette {
            unimplemented!();
        }
        fn domain_id(&self) -> DomainId {
            0
        }
        fn is_remote_mirror(&self) -> bool {
            self.remote_mirror
        }
        fn is_mouse_grabbed(&self) -> bool {
            false
        }
        fn is_alt_screen_active(&self) -> bool {
            false
        }
        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<Url> {
            None
        }
    }

    fn now_unix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn contract_only_pane_classifies_from_user_var() {
        let pane = FakeAgentPane::new(None);
        pane.set_contract(&format!(
            "v1;agent=soul;state=working;session=s-1;ts={}",
            now_unix()
        ));
        assert!(evaluate_pane(pane.as_ref() as &dyn Pane));
        let status = status_for_pane(pane.pane_id()).expect("classified");
        assert_eq!(status.agent_id, "soul");
        assert_eq!(status.state, AgentState::Working);
        assert_eq!(status.evidence, AgentEvidence::Contract);
        assert_eq!(status.session_id.as_deref(), Some("s-1"));
        evict_pane(pane.pane_id());
    }

    #[test]
    fn screen_rules_classify_a_process_identified_pane() {
        let pane = FakeAgentPane::new(Some("/usr/local/bin/claude"));
        pane.set_screen("some output\n⏵⏵ Cooking… (esc to interrupt · 12s)\n");
        assert!(evaluate_pane(pane.as_ref() as &dyn Pane));
        let status = status_for_pane(pane.pane_id()).expect("classified");
        assert_eq!(status.agent_id, "claude");
        assert_eq!(status.state, AgentState::Working);
        assert_eq!(status.evidence, AgentEvidence::Screen);
        evict_pane(pane.pane_id());
    }

    /// An npm-installed agent runs as `node <shim>`: the leader executable
    /// says only "node", and the agent's name rides in argv. This is the
    /// installation style the basename-only identifier was blind to.
    #[test]
    fn interpreter_wrapped_agent_identifies_through_argv() {
        let pane = FakeAgentPane::new(Some("/opt/homebrew/bin/node"));
        pane.set_argv(&["node", "/opt/homebrew/bin/claude"]);
        pane.set_screen("some output\n⏵⏵ Cooking… (esc to interrupt · 12s)\n");
        assert!(evaluate_pane(pane.as_ref() as &dyn Pane));
        let status = status_for_pane(pane.pane_id()).expect("classified");
        assert_eq!(status.agent_id, "claude");
        assert_eq!(status.state, AgentState::Working);
        assert_eq!(status.evidence, AgentEvidence::Screen);
        evict_pane(pane.pane_id());

        // A plain interpreter with no agent in its argv stays unidentified.
        let plain = FakeAgentPane::new(Some("/opt/homebrew/bin/node"));
        plain.set_argv(&["node", "/opt/tools/build.js"]);
        plain.set_screen("some output\n⏵⏵ Cooking… (esc to interrupt · 12s)\n");
        assert!(!evaluate_pane(plain.as_ref() as &dyn Pane));
        assert!(status_for_pane(plain.pane_id()).is_none());
        evict_pane(plain.pane_id());

        // A retitled interpreter (`process.title = "claude"` clobbered the
        // argv block) identifies by the surviving argv[0].
        let retitled = FakeAgentPane::new(Some("/opt/homebrew/Cellar/node/26.7.0/bin/node"));
        retitled.set_argv(&["claude"]);
        retitled.set_screen("some output\n⏵⏵ Cooking… (esc to interrupt · 12s)\n");
        assert!(evaluate_pane(retitled.as_ref() as &dyn Pane));
        let status = status_for_pane(retitled.pane_id()).expect("classified");
        assert_eq!(status.agent_id, "claude");
        evict_pane(retitled.pane_id());
    }

    /// The argv read is lazy: a leader that names the agent directly must
    /// never pay for it, and an interpreter leader pays exactly once per
    /// identity-cache window, not once per evaluation.
    #[test]
    fn argv_is_fetched_only_for_interpreter_leaders() {
        let direct = FakeAgentPane::new(Some("/usr/local/bin/claude"));
        direct.set_screen("some output\n⏵⏵ Cooking… (esc to interrupt · 12s)\n");
        assert!(evaluate_pane(direct.as_ref() as &dyn Pane));
        assert_eq!(direct.argv_probes.load(Ordering::SeqCst), 0);
        evict_pane(direct.pane_id());

        let wrapped = FakeAgentPane::new(Some("/usr/bin/node"));
        wrapped.set_argv(&["node", "/opt/homebrew/bin/claude"]);
        wrapped.set_screen("some output\n⏵⏵ Cooking… (esc to interrupt · 12s)\n");
        assert!(evaluate_pane(wrapped.as_ref() as &dyn Pane));
        assert_eq!(wrapped.argv_probes.load(Ordering::SeqCst), 1);
        // A second evaluation inside the identity TTL serves the cached
        // identity without another argv read (the unchanged verdict means
        // evaluate_pane reports no state change — irrelevant here).
        wrapped.set_screen("more output\n⏵⏵ Cooking… (esc to interrupt · 13s)\n");
        let _ = evaluate_pane(wrapped.as_ref() as &dyn Pane);
        assert_eq!(wrapped.argv_probes.load(Ordering::SeqCst), 1);
        evict_pane(wrapped.pane_id());
    }

    /// A self-updating agent replaces its own binary; Linux then reports
    /// the leader as `/path/claude (deleted)`. Identification must survive
    /// that, or detection silently dies right after every agent update.
    #[test]
    fn replaced_binary_still_identifies_as_its_agent() {
        let pane = FakeAgentPane::new(Some("/usr/local/bin/claude (deleted)"));
        pane.set_screen("some output\n⏵⏵ Cooking… (esc to interrupt · 12s)\n");
        assert!(evaluate_pane(pane.as_ref() as &dyn Pane));
        let status = status_for_pane(pane.pane_id()).expect("classified");
        assert_eq!(status.agent_id, "claude");
        assert_eq!(status.evidence, AgentEvidence::Screen);
        evict_pane(pane.pane_id());
    }

    /// codex draws its trust prompt near the top of the screen and leaves
    /// the rest blank; on a 40-row terminal the prompt must still reach
    /// the `top_non_empty_lines` rules rather than falling outside the
    /// slice of the screen the engine is given.
    #[test]
    fn tall_screen_top_prompt_reaches_top_region_rules() {
        let pane = FakeAgentPane::new(Some("/usr/local/bin/codex"));
        let mut screen = String::from(
            "> You are in /Users/someone/project\n\
             \n\
             Do you trust the contents of this directory?\n\
             \n\
             > 1. Yes, continue\n\
             2. No, quit\n",
        );
        screen.push_str(&"\n".repeat(34));
        pane.set_screen(&screen);
        drain_step(pane.as_ref() as &dyn Pane);
        let status = status_for_pane(pane.pane_id()).expect("classified");
        assert_eq!(
            status.state,
            AgentState::Blocked,
            "a trust prompt at the top of a tall screen must classify as blocked"
        );
        evict_pane(pane.pane_id());
    }

    /// Output that lands between the screen read and the verdict being
    /// cached must not pin the stale verdict to the post-write seqno: a
    /// pane that then blocks waiting for input emits nothing further, so
    /// the seqno never moves again and the reuse clause would serve the
    /// stale verdict forever.
    #[test]
    fn output_landing_during_evaluation_does_not_pin_stale_verdict() {
        let blocked_screen = "Do you want to proceed?\n> 1. Yes\n2. No, and esc to cancel\n";
        // Sanity: the prompt text below classifies as blocked when read
        // normally, so a later failure means staleness, not a bad fixture.
        let probe = FakeAgentPane::new(Some("/usr/local/bin/claude"));
        probe.set_screen(blocked_screen);
        drain_step(probe.as_ref() as &dyn Pane);
        assert_eq!(
            status_for_pane(probe.pane_id()).expect("probe classified").state,
            AgentState::Blocked
        );
        evict_pane(probe.pane_id());

        let pane = FakeAgentPane::new(Some("/usr/local/bin/claude"));
        pane.set_screen("some output\n⏵⏵ Cooking… (esc to interrupt · 12s)\n");
        // The prompt lands while the engine is still evaluating the old
        // screen: the read returns the working screen, then the write
        // applies and the seqno advances before the verdict is cached.
        pane.set_screen_after_next_read(blocked_screen);
        drain_step(pane.as_ref() as &dyn Pane);
        // This round legitimately reports the pre-write screen.
        assert_eq!(
            status_for_pane(pane.pane_id()).expect("classified").state,
            AgentState::Working
        );
        // The pane now waits for input and emits nothing further. Later
        // rounds must notice the prompt instead of serving the stale
        // verdict; allow a few rounds for any guarded transition.
        let mut state = AgentState::Working;
        for _ in 0..4 {
            drain_step_next_round(pane.as_ref() as &dyn Pane);
            state = status_for_pane(pane.pane_id()).expect("classified").state;
            if state == AgentState::Blocked {
                break;
            }
        }
        assert_eq!(
            state,
            AgentState::Blocked,
            "the stale pre-write verdict must not survive on an unmoving seqno"
        );
        evict_pane(pane.pane_id());
    }

    #[test]
    fn screen_verdict_overrides_contract_working() {
        let pane = FakeAgentPane::new(None);
        pane.set_contract(&format!(
            "v1;agent=claude;state=working;ts={}",
            now_unix()
        ));
        // The Esc-interrupt / permission-cancel shape: contract says
        // working, the screen shows the idle prompt box.
        pane.set_screen(
            "previous output\n──────────────────────────────\n❯\n──────────────────────────────\n  ? for shortcuts\n",
        );
        evaluate_pane(pane.as_ref() as &dyn Pane);
        let status = status_for_pane(pane.pane_id()).expect("classified");
        assert_eq!(status.state, AgentState::Idle);
        assert_eq!(status.evidence, AgentEvidence::Screen);
        evict_pane(pane.pane_id());
    }

    #[test]
    fn unidentified_pane_is_never_classified() {
        let pane = FakeAgentPane::new(Some("/bin/zsh"));
        pane.set_screen("~/src\n❯ \n");
        assert!(!evaluate_pane(pane.as_ref() as &dyn Pane));
        assert!(status_for_pane(pane.pane_id()).is_none());
    }

    #[test]
    fn evict_pane_clears_the_registry() {
        let pane = FakeAgentPane::new(None);
        pane.set_contract(&format!("v1;agent=soul;state=idle;ts={}", now_unix()));
        evaluate_pane(pane.as_ref() as &dyn Pane);
        assert!(status_for_pane(pane.pane_id()).is_some());
        assert!(evict_pane(pane.pane_id()), "a published record was dropped");
        assert!(status_for_pane(pane.pane_id()).is_none());
    }

    /// The PaneRemoved subscriber must evict silently: the notification
    /// already tells clients, and publishing from inside Mux::notify
    /// deadlocks the subscriber lock.
    #[test]
    fn pane_removed_evicts_without_publishing() {
        let pane = FakeAgentPane::new(None);
        pane.set_contract(&format!("v1;agent=soul;state=working;ts={}", now_unix()));
        drain_step(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 1);
        // The subscriber body: evict and deliberately drop the result.
        evict_pane(pane.pane_id());
        assert_eq!(publishes_for(pane.pane_id()), 1, "eviction must not publish");
        assert!(status_for_pane(pane.pane_id()).is_none());
    }

    const CODEX_QUESTION: &str = "\u{203a} fix the bug\n\n  Which approach?\n  \u{276f} 1. A\n    2. B\n\n  press enter to confirm or esc to cancel\n";
    const CLAUDE_QUESTION: &str = "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n Do you want to proceed?\n \u{276f} 1. Yes\n 2. No\n esc to cancel \u{b7} enter to confirm\n";
    const CLAUDE_PROMPT_BOX: &str = "previous output\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\u{276f}\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n  ? for shortcuts\n";
    const CLAUDE_WORKING: &str = "some output\n\u{23f5}\u{23f5} Cooking\u{2026} (esc to interrupt \u{b7} 12s)\n";

    /// The user's original bug: switching an option in codex's question
    /// form redraws the widget, one evaluation lands on the half-drawn
    /// frame, and the published state used to flap Blocked→Idle→Blocked —
    /// replaying the needs-input sound each time.
    #[test]
    fn codex_option_switch_stays_blocked() {
        let pane = FakeAgentPane::new(Some("/opt/homebrew/bin/codex"));
        pane.set_screen(CODEX_QUESTION);
        drain_step(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 1);
        assert_eq!(
            status_for_pane(pane.pane_id()).map(|s| s.state),
            Some(AgentState::Blocked)
        );
        // Mid-redraw frame: the widget is briefly absent.
        pane.set_screen("\u{203a} fix the bug\n");
        drain_step_next_round(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 1, "transient frame is held");
        assert_eq!(
            status_for_pane(pane.pane_id()).map(|s| s.state),
            Some(AgentState::Blocked)
        );
        // Redraw complete: same question, still one waiting episode.
        pane.set_screen(CODEX_QUESTION);
        drain_step_next_round(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 1, "no republish, no re-ring");
        assert_eq!(
            status_for_pane(pane.pane_id()).map(|s| s.state),
            Some(AgentState::Blocked)
        );
        evict_pane(pane.pane_id());
    }

    /// Answering a claude permission form passes through a frame that
    /// reads as idle before the spinner appears; that frame must never be
    /// published (it used to clear the GUI's attention latch and announce
    /// premature completion).
    #[test]
    fn claude_answer_completion_never_publishes_idle() {
        let pane = FakeAgentPane::new(Some("/usr/local/bin/claude"));
        pane.set_screen(CLAUDE_QUESTION);
        drain_step(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 1);
        assert_eq!(
            status_for_pane(pane.pane_id()).map(|s| s.state),
            Some(AgentState::Blocked)
        );
        pane.set_screen(CLAUDE_PROMPT_BOX);
        drain_step_next_round(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 1, "idle frame is held");
        for _ in 0..4 {
            pane.set_screen(CLAUDE_WORKING);
            drain_step_next_round(pane.as_ref() as &dyn Pane);
            assert_ne!(
                status_for_pane(pane.pane_id()).map(|s| s.state),
                Some(AgentState::Idle),
                "idle must never surface"
            );
        }
        assert_eq!(
            status_for_pane(pane.pane_id()).map(|s| s.state),
            Some(AgentState::Working)
        );
        assert_eq!(publishes_for(pane.pane_id()), 2, "exactly blocked then working");
        evict_pane(pane.pane_id());
    }

    /// The claude-relic case behind the `cat` misreport: once the agent
    /// process is gone and no contract vouches for the pane, it must stop
    /// being classified entirely.
    #[test]
    fn process_gone_without_contract_is_evicted() {
        let pane = FakeAgentPane::new(Some("/usr/local/bin/claude"));
        pane.set_screen("some output\n\u{23f5}\u{23f5} Cooking\u{2026} (esc to interrupt \u{b7} 12s)\n");
        drain_step(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 1);
        // claude exits; the shell takes the foreground back and prints
        // text that would match claude's blocked rules.
        pane.set_process(Some("/bin/zsh"));
        pane.set_screen("do you want to proceed? yes\nwaiting for permission\n");
        drain_step(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 2, "eviction is published");
        assert!(status_for_pane(pane.pane_id()).is_none());
    }

    /// A live process is ground truth: a relic contract from a previous
    /// occupant neither renames the agent nor decorates it with sessions.
    #[test]
    fn process_identity_overrides_contract_agent() {
        let pane = FakeAgentPane::new(Some("/opt/homebrew/bin/codex"));
        pane.set_contract(&format!(
            "v1;agent=soul;state=working;session=s-9;ts={}",
            now_unix()
        ));
        drain_step(pane.as_ref() as &dyn Pane);
        let status = status_for_pane(pane.pane_id()).expect("classified");
        assert_eq!(status.agent_id, "codex");
        assert_eq!(status.session_id, None);
        evict_pane(pane.pane_id());
    }

    /// An `ended=1` farewell is ignored while the process is still the
    /// pane's foreground: it was premature, and hiding a live agent from
    /// the panel would be wrong.
    #[test]
    fn ended_contract_with_live_process_keeps_screen_rules() {
        let pane = FakeAgentPane::new(Some("/usr/local/bin/claude"));
        pane.set_contract(&format!("v1;agent=claude;state=idle;ts={};ended=1", now_unix()));
        pane.set_screen("some output\n\u{23f5}\u{23f5} Cooking\u{2026} (esc to interrupt \u{b7} 12s)\n");
        drain_step(pane.as_ref() as &dyn Pane);
        let status = status_for_pane(pane.pane_id()).expect("classified");
        assert_eq!(status.state, AgentState::Working);
        assert!(!status.ended);
        evict_pane(pane.pane_id());
    }

    /// The soul case: a native emitter may sit silent long past contract
    /// freshness. Its identity survives while the pane's foreground
    /// leader is unchanged, and dies with it.
    #[test]
    fn contract_survives_while_leader_is_unchanged() {
        let pane = FakeAgentPane::new(Some("/usr/local/bin/soul"));
        pane.set_contract(&format!("v1;agent=soul;state=working;ts={}", now_unix()));
        drain_step(pane.as_ref() as &dyn Pane);
        assert_eq!(
            status_for_pane(pane.pane_id()).map(|s| s.agent_id),
            Some("soul".to_string())
        );
        // Hours pass: the contract re-states something ancient, but the
        // same soul binary still owns the pty.
        pane.set_contract(&format!(
            "v1;agent=soul;state=working;ts={}",
            now_unix() - 8 * 60 * 60
        ));
        drain_step(pane.as_ref() as &dyn Pane);
        assert_eq!(
            status_for_pane(pane.pane_id()).map(|s| s.agent_id),
            Some("soul".to_string()),
            "unchanged leader keeps the stale contract's identity alive"
        );
        // soul exits: the shell is foreground again, the stale contract
        // is now a relic.
        pane.set_process(Some("/bin/zsh"));
        drain_step(pane.as_ref() as &dyn Pane);
        assert!(status_for_pane(pane.pane_id()).is_none());
    }

    /// A contract-backed agent that dies without `ended=1` must not pin
    /// its identity onto the shell: even a *fresh* contract (idle ones
    /// never expire) dies with its recorded foreground leader.
    #[test]
    fn fresh_idle_contract_dies_with_its_leader() {
        let pane = FakeAgentPane::new(Some("/usr/local/bin/soul"));
        pane.set_contract(&format!("v1;agent=soul;state=idle;ts={}", now_unix()));
        drain_step(pane.as_ref() as &dyn Pane);
        assert!(status_for_pane(pane.pane_id()).is_some());
        // soul crashes; the shell takes the foreground back.
        pane.set_process(Some("/bin/zsh"));
        drain_step(pane.as_ref() as &dyn Pane);
        assert!(
            status_for_pane(pane.pane_id()).is_none(),
            "a dead emitter's contract must not survive on freshness alone"
        );
    }

    /// A transient probe failure (leader reads as None) must not evict:
    /// only a definite leader change is proof the emitter is gone.
    #[test]
    fn probe_failure_does_not_evict_a_contract_identity() {
        let pane = FakeAgentPane::new(Some("/usr/local/bin/soul"));
        pane.set_contract(&format!("v1;agent=soul;state=working;ts={}", now_unix()));
        drain_step(pane.as_ref() as &dyn Pane);
        assert!(status_for_pane(pane.pane_id()).is_some());
        pane.set_process(None);
        drain_step(pane.as_ref() as &dyn Pane);
        assert!(
            status_for_pane(pane.pane_id()).is_some(),
            "an unobservable leader is unknown, not dead"
        );
    }

    /// Reloading rules must drop cached screen verdicts: an unchanged
    /// screen (same seqno) would otherwise keep serving conclusions
    /// computed under the old rules indefinitely.
    #[test]
    fn reload_rules_clears_cached_screen_verdicts() {
        let pane = FakeAgentPane::new(Some("/usr/local/bin/claude"));
        pane.set_screen(CLAUDE_WORKING);
        drain_step(pane.as_ref() as &dyn Pane);
        assert!(REGISTRY
            .read()
            .get(&pane.pane_id())
            .and_then(|r| r.screen_cache.as_ref())
            .is_some());
        reload_rules();
        assert!(
            REGISTRY
                .read()
                .get(&pane.pane_id())
                .and_then(|r| r.screen_cache.as_ref())
                .is_none(),
            "old-rule verdicts must not survive a reload"
        );
        evict_pane(pane.pane_id());
    }

    /// A relic contract from a previous occupant must not tear the record
    /// down every round: that would republish per round and destroy the
    /// hysteresis window, resurrecting the repeated needs-input ring.
    #[test]
    fn relic_contract_does_not_thrash_the_record() {
        let pane = FakeAgentPane::new(Some("/opt/homebrew/bin/codex"));
        pane.set_contract(&format!(
            "v1;agent=soul;state=working;session=s-9;ts={}",
            now_unix() - 60
        ));
        pane.set_screen(CODEX_QUESTION);
        drain_step(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 1);
        // The mid-redraw frame must still be held: the relic contract's
        // session must not have destroyed the pending window.
        pane.set_screen("\u{203a} fix the bug\n");
        drain_step_next_round(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 1, "hysteresis survives the relic");
        pane.set_screen(CODEX_QUESTION);
        drain_step_next_round(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 1, "no per-round republish");
        assert_eq!(
            status_for_pane(pane.pane_id()).map(|s| s.state),
            Some(AgentState::Blocked)
        );
        evict_pane(pane.pane_id());
    }

    /// A pane that never runs an agent keeps its negative identification
    /// cached: repeated evaluations must not re-probe the process table.
    #[test]
    fn non_agent_pane_probes_are_cached() {
        let pane = FakeAgentPane::new(Some("/bin/zsh"));
        for _ in 0..3 {
            drain_step(pane.as_ref() as &dyn Pane);
        }
        assert_eq!(
            pane.probes.load(Ordering::SeqCst),
            1,
            "negative identification is cached for the TTL"
        );
    }

    /// Working → Idle publishes only after consecutive confirmations.
    #[test]
    fn working_to_idle_confirms_before_publishing() {
        let pane = FakeAgentPane::new(Some("/usr/local/bin/claude"));
        pane.set_screen(CLAUDE_WORKING);
        drain_step(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 1);
        for expect_published in [1, 1, 1, 2] {
            pane.set_screen(CLAUDE_PROMPT_BOX);
            drain_step_next_round(pane.as_ref() as &dyn Pane);
            assert_eq!(publishes_for(pane.pane_id()), expect_published);
        }
        assert_eq!(
            status_for_pane(pane.pane_id()).map(|s| s.state),
            Some(AgentState::Idle)
        );
        evict_pane(pane.pane_id());
    }

    /// A displayable title exists on every pane, but only real OSC
    /// emissions may drive the manifests' title/progress rules: with zero
    /// evidence and an empty screen, claude must fall through to the
    /// Idle fallback (leaving the contract tier reachable), and codex's
    /// `osc_title_idle` (`\S`) must not fire.
    #[test]
    fn empty_osc_evidence_reaches_the_fallback_tier() {
        for process in ["/usr/local/bin/claude", "/opt/homebrew/bin/codex"] {
            let pane = FakeAgentPane::new(Some(process));
            *pane.title.lock() = "zsh".to_string();
            evaluate_pane(pane.as_ref() as &dyn Pane);
            let status = status_for_pane(pane.pane_id()).expect("classified");
            assert_eq!(status.evidence, AgentEvidence::Fallback, "{process}");
            assert_eq!(status.state, AgentState::Idle, "{process}");
            evict_pane(pane.pane_id());
        }
    }

    /// The same codex pane with a real (spinner-free) OSC title hits the
    /// manifest's `osc_title_idle` rule: evidence flows once it exists.
    #[test]
    fn real_osc_title_feeds_the_screen_rules() {
        let pane = FakeAgentPane::new(Some("/opt/homebrew/bin/codex"));
        pane.set_osc_title("auto_order_sys");
        evaluate_pane(pane.as_ref() as &dyn Pane);
        let status = status_for_pane(pane.pane_id()).expect("classified");
        assert_eq!(status.evidence, AgentEvidence::Screen);
        assert_eq!(status.state, AgentState::Idle);
        evict_pane(pane.pane_id());
    }

    /// A pane whose agent goes away publishes the eviction exactly once
    /// when it happens through the evaluation path.
    #[test]
    fn dropped_agent_publishes_once() {
        let pane = FakeAgentPane::new(None);
        pane.set_contract(&format!("v1;agent=soul;state=working;ts={}", now_unix()));
        drain_step(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 1);
        pane.clear_contract();
        drain_step(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 2, "the drop is published");
        drain_step(pane.as_ref() as &dyn Pane);
        assert_eq!(publishes_for(pane.pane_id()), 2, "and only once");
        assert!(status_for_pane(pane.pane_id()).is_none());
    }

    #[test]
    fn republish_only_on_change() {
        let pane = FakeAgentPane::new(None);
        pane.set_contract(&format!("v1;agent=soul;state=working;ts={}", now_unix()));
        assert!(evaluate_pane(pane.as_ref() as &dyn Pane));
        // Same inputs, same status: no republish.
        assert!(!evaluate_pane(pane.as_ref() as &dyn Pane));
        pane.set_contract(&format!("v1;agent=soul;state=blocked;ts={}", now_unix()));
        assert!(evaluate_pane(pane.as_ref() as &dyn Pane));
        assert_eq!(
            status_for_pane(pane.pane_id()).map(|s| s.state),
            Some(AgentState::Blocked)
        );
        evict_pane(pane.pane_id());
    }
}
