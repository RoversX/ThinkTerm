//! Manifest-driven screen detection engine.
//!
//! Evaluates per-agent TOML rule manifests against a snapshot of a pane's
//! live screen tail plus its OSC title/progress state. The manifest format
//! is compatible with herdr's agent-detection manifests
//! (<https://github.com/herdrdev/herdr>, Apache-2.0) so their maintained
//! rule files can be dropped in unchanged; this engine implements the
//! feature subset those files use.

use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use parking_lot::RwLock;
use thinkterm_proto::AgentState;

/// Everything a manifest evaluation can look at. OSC regions source from
/// their dedicated fields; all other regions slice `screen`.
pub struct DetectionInput<'a> {
    pub screen: &'a str,
    pub osc_title: &'a str,
    pub osc_progress: &'a str,
}

/// Outcome of running one agent's manifest over a snapshot.
pub enum ScreenVerdict {
    /// A rule matched and assigns a state.
    State(AgentState),
    /// A `skip_state_update` rule matched (transcript viewer and friends):
    /// keep whatever state was published before.
    Freeze,
    /// No rule matched. Known agents fall back towards Idle at the caller's
    /// discretion (never Blocked), mirroring herdr's
    /// `default_known_agent_idle_fallback`.
    NoMatch,
}

// No `deny_unknown_fields` at the file level, matching herdr: future
// manifest revisions may add top-level metadata, and a drop-in herdr
// file must keep loading. Rules and gates below stay strict — that is
// where a typo'd key silently changes matching behavior.
#[derive(Deserialize)]
struct ManifestFile {
    id: String,
    #[allow(dead_code)]
    version: Option<String>,
    min_engine_version: Option<u32>,
    #[allow(dead_code)]
    updated_at: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    rules: Vec<ManifestRule>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct ManifestRule {
    id: String,
    state: Option<ManifestState>,
    #[serde(default)]
    priority: i32,
    #[serde(default = "default_region")]
    region: String,
    #[serde(default)]
    skip_state_update: bool,
    // Parsed for manifest compatibility; the arbitration layer does not
    // consume the visibility hints yet.
    #[serde(default)]
    #[allow(dead_code)]
    visible_idle: bool,
    #[serde(default)]
    #[allow(dead_code)]
    visible_blocker: bool,
    #[serde(default)]
    #[allow(dead_code)]
    visible_working: bool,
    #[serde(default)]
    all: Vec<ManifestGate>,
    #[serde(default)]
    any: Vec<ManifestGate>,
    #[serde(default, rename = "not")]
    not_gate: Vec<ManifestGate>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct ManifestGate {
    #[serde(default)]
    all: Vec<ManifestGate>,
    #[serde(default)]
    any: Vec<ManifestGate>,
    #[serde(default, rename = "not")]
    not_gate: Vec<ManifestGate>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
}

#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ManifestState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

impl From<ManifestState> for AgentState {
    fn from(value: ManifestState) -> Self {
        match value {
            ManifestState::Idle => AgentState::Idle,
            ManifestState::Working => AgentState::Working,
            ManifestState::Blocked => AgentState::Blocked,
            ManifestState::Unknown => AgentState::Unknown,
        }
    }
}

fn default_region() -> String {
    "whole_recent".to_string()
}

struct CompiledGate {
    all: Vec<CompiledGate>,
    any: Vec<CompiledGate>,
    not_gate: Vec<CompiledGate>,
    /// Pre-lowercased needles matched against the lowercased region text.
    contains: Vec<String>,
    regex: Vec<Regex>,
    line_regex: Vec<Regex>,
}

struct CompiledRule {
    #[allow(dead_code)]
    id: String,
    state: Option<AgentState>,
    priority: i32,
    region: String,
    skip_state_update: bool,
    gate: CompiledGate,
}

pub(crate) struct CompiledManifest {
    pub(crate) id: String,
    pub(crate) aliases: Vec<String>,
    rules: Vec<CompiledRule>,
}

/// Bundled manifests, taken from herdr's maintained agent-detection set
/// (Apache-2.0; see the attribution header inside each file).
const BUNDLED_MANIFESTS: &[&str] = &[
    include_str!("manifests/claude.toml"),
    include_str!("manifests/codex.toml"),
    include_str!("manifests/github-copilot.toml"),
    include_str!("manifests/cursor.toml"),
    include_str!("manifests/pi.toml"),
    include_str!("manifests/opencode.toml"),
    include_str!("manifests/kimi.toml"),
];

static MANIFESTS: RwLock<Option<HashMap<String, CompiledManifest>>> = RwLock::new(None);
/// Bumped whenever the manifest map is (re)loaded. Cached screen verdicts
/// record the generation they were computed under, so a reload invalidates
/// them even when a racing evaluation re-caches after the cache sweep.
static MANIFEST_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn manifest_generation() -> u64 {
    MANIFEST_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

/// The user-override directory: a `<id>.toml` here replaces the bundled
/// manifest with the same id wholesale. Lives next to settings.json (see
/// `native_settings::settings_path`).
fn override_dir() -> std::path::PathBuf {
    // Unit tests must not read the developer's real override directory: a
    // legitimately installed user override (the exact file the settings
    // page tells users to create) would silently change every bundled-
    // manifest assertion and turn the suite red on that machine only.
    #[cfg(test)]
    {
        return std::path::PathBuf::from("/nonexistent-thinkterm-test-agent-detection");
    }
    #[cfg(not(test))]
    config::HOME_DIR
        .join(".config")
        .join("thinkterm")
        .join("agent-detection")
}

/// The manifest feature set this engine implements; the herdr manifests
/// we bundle declare requirements up to this.
const ENGINE_VERSION: u32 = 3;

fn compile_manifest(text: &str, origin: &str) -> Option<CompiledManifest> {
    let file: ManifestFile = match toml::from_str(text) {
        Ok(file) => file,
        Err(err) => {
            log::error!("agent manifest {origin} failed to parse: {err:#}");
            return None;
        }
    };
    if let Some(required) = file.min_engine_version {
        if required > ENGINE_VERSION {
            // Evaluating a manifest that needs features we lack would
            // produce silently wrong verdicts, which is worse than no
            // manifest at all.
            log::warn!(
                "agent manifest {origin} requires engine version {required} \
                 but this build implements {ENGINE_VERSION}; skipping it"
            );
            return None;
        }
    }
    if let Err(err) = validate_manifest(&file) {
        log::error!("agent manifest {origin} is invalid: {err}");
        return None;
    }
    let mut rules = Vec::with_capacity(file.rules.len());
    for rule in &file.rules {
        let gate = match compile_gate(&manifest_gate_from_rule(rule)) {
            Ok(gate) => gate,
            Err(err) => {
                log::error!(
                    "agent manifest {origin} rule {} failed to compile: {err}",
                    rule.id
                );
                continue;
            }
        };
        rules.push(CompiledRule {
            id: rule.id.clone(),
            state: rule.state.map(AgentState::from),
            priority: rule.priority,
            region: rule.region.trim().to_string(),
            skip_state_update: rule.skip_state_update,
            gate,
        });
    }
    Some(CompiledManifest {
        id: file.id,
        aliases: file.aliases,
        rules,
    })
}

fn compile_gate(gate: &ManifestGate) -> Result<CompiledGate, String> {
    Ok(CompiledGate {
        all: gate
            .all
            .iter()
            .map(compile_gate)
            .collect::<Result<_, _>>()?,
        any: gate
            .any
            .iter()
            .map(compile_gate)
            .collect::<Result<_, _>>()?,
        not_gate: gate
            .not_gate
            .iter()
            .map(compile_gate)
            .collect::<Result<_, _>>()?,
        contains: gate.contains.iter().map(|s| s.to_lowercase()).collect(),
        regex: gate
            .regex
            .iter()
            .map(|p| Regex::new(p).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?,
        line_regex: gate
            .line_regex
            .iter()
            .map(|p| Regex::new(p).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?,
    })
}

// ---------------------------------------------------------------------------
// Load-time validation. A manifest that fails here is dropped whole (for a
// user override the bundled one keeps serving), so a typo'd field or region
// surfaces as an error instead of a rule that silently never fires — or
// worse, a rule that always fires because an unknown region evaluates
// against "". Checks and limits mirror herdr's `validate_manifest`.
// ---------------------------------------------------------------------------

const MAX_RULES_PER_MANIFEST: usize = 128;
const MAX_GATE_DEPTH: usize = 8;
const MAX_TOTAL_GATES: usize = 512;
const MAX_MATCHERS_PER_GATE: usize = 32;
const MAX_TOTAL_MATCHERS: usize = 1024;
const MAX_MATCHER_CHARS: usize = 512;
/// `top_non_empty_lines` was added in engine v3; a rule using it under a
/// lower `min_engine_version` banner would evaluate against "" on the older
/// engines that banner admits.
const TOP_NON_EMPTY_LINES_ENGINE_VERSION: u32 = 3;

#[derive(Default)]
struct ManifestComplexity {
    total_gates: usize,
    total_matchers: usize,
}

fn validate_manifest(file: &ManifestFile) -> Result<(), String> {
    if file.rules.is_empty() {
        return Err("manifest must contain at least one rule".to_string());
    }
    if file.rules.len() > MAX_RULES_PER_MANIFEST {
        return Err(format!(
            "manifest contains {} rules, max is {MAX_RULES_PER_MANIFEST}",
            file.rules.len()
        ));
    }

    let mut complexity = ManifestComplexity::default();
    for rule in &file.rules {
        if rule.id.trim().is_empty() {
            return Err("manifest rule id must not be empty".to_string());
        }
        if rule.skip_state_update {
            if rule.state != Some(ManifestState::Unknown) {
                return Err(format!(
                    "rule {} uses skip_state_update without state = \"unknown\"",
                    rule.id
                ));
            }
            if rule.visible_idle || rule.visible_blocker || rule.visible_working {
                return Err(format!(
                    "rule {} uses skip_state_update with visible state evidence",
                    rule.id
                ));
            }
        }
        validate_region_name(&rule.region)
            .map_err(|err| format!("rule {} uses invalid region: {err}", rule.id))?;
        if rule.region.trim().starts_with("top_non_empty_lines(")
            && file
                .min_engine_version
                .is_some_and(|version| version < TOP_NON_EMPTY_LINES_ENGINE_VERSION)
        {
            return Err(format!(
                "rule {} uses top_non_empty_lines but min_engine_version is below {}",
                rule.id, TOP_NON_EMPTY_LINES_ENGINE_VERSION
            ));
        }
        validate_gate(&manifest_gate_from_rule(rule), "rule", 0, &mut complexity)
            .map_err(|err| format!("rule {} has invalid matcher gates: {err}", rule.id))?;
    }

    Ok(())
}

fn validate_gate(
    gate: &ManifestGate,
    context: &str,
    depth: usize,
    complexity: &mut ManifestComplexity,
) -> Result<(), String> {
    if depth > MAX_GATE_DEPTH {
        return Err(format!("{context} exceeds max gate depth {MAX_GATE_DEPTH}"));
    }
    complexity.total_gates += 1;
    if complexity.total_gates > MAX_TOTAL_GATES {
        return Err(format!("manifest exceeds max gate count {MAX_TOTAL_GATES}"));
    }
    validate_matcher_limits(gate, context, complexity)?;
    if !gate_has_positive_matcher(gate) {
        return Err(format!("{context} must contain a positive matcher"));
    }
    validate_regex_patterns(&gate.regex, context, "regex")?;
    validate_regex_patterns(&gate.line_regex, context, "line_regex")?;
    for nested in &gate.all {
        validate_gate(nested, "all gate", depth + 1, complexity)?;
    }
    for nested in &gate.any {
        validate_gate(nested, "any gate", depth + 1, complexity)?;
    }
    for nested in &gate.not_gate {
        if !gate_has_any_matcher(nested) {
            return Err(format!("{context} contains an empty not gate"));
        }
        validate_not_gate(nested, depth + 1, complexity)?;
    }
    Ok(())
}

/// A `not` gate needs no positive matcher of its own — its job is to name
/// what must be absent — so it gets looser structural checks than
/// `validate_gate`.
fn validate_not_gate(
    gate: &ManifestGate,
    depth: usize,
    complexity: &mut ManifestComplexity,
) -> Result<(), String> {
    if depth > MAX_GATE_DEPTH {
        return Err(format!("not gate exceeds max gate depth {MAX_GATE_DEPTH}"));
    }
    complexity.total_gates += 1;
    if complexity.total_gates > MAX_TOTAL_GATES {
        return Err(format!("manifest exceeds max gate count {MAX_TOTAL_GATES}"));
    }
    validate_matcher_limits(gate, "not gate", complexity)?;
    if !gate_has_any_matcher(gate) {
        return Err("not gate must contain a matcher".to_string());
    }
    validate_regex_patterns(&gate.regex, "not gate", "regex")?;
    validate_regex_patterns(&gate.line_regex, "not gate", "line_regex")?;
    for nested in &gate.all {
        validate_gate(nested, "not all gate", depth + 1, complexity)?;
    }
    for nested in &gate.any {
        validate_gate(nested, "not any gate", depth + 1, complexity)?;
    }
    for nested in &gate.not_gate {
        validate_not_gate(nested, depth + 1, complexity)?;
    }
    Ok(())
}

fn validate_matcher_limits(
    gate: &ManifestGate,
    context: &str,
    complexity: &mut ManifestComplexity,
) -> Result<(), String> {
    let matcher_count = gate.contains.len() + gate.regex.len() + gate.line_regex.len();
    if matcher_count > MAX_MATCHERS_PER_GATE {
        return Err(format!(
            "{context} has {matcher_count} direct matchers, max is {MAX_MATCHERS_PER_GATE}"
        ));
    }
    complexity.total_matchers += matcher_count;
    if complexity.total_matchers > MAX_TOTAL_MATCHERS {
        return Err(format!(
            "manifest exceeds max matcher count {MAX_TOTAL_MATCHERS}"
        ));
    }
    for value in gate
        .contains
        .iter()
        .chain(gate.regex.iter())
        .chain(gate.line_regex.iter())
    {
        if value.chars().count() > MAX_MATCHER_CHARS {
            return Err(format!(
                "{context} matcher exceeds max length {MAX_MATCHER_CHARS}"
            ));
        }
    }
    Ok(())
}

fn validate_regex_patterns(patterns: &[String], context: &str, field: &str) -> Result<(), String> {
    for pattern in patterns {
        Regex::new(pattern).map_err(|err| {
            format!("{context} contains invalid {field} pattern {pattern:?}: {err}")
        })?;
    }
    Ok(())
}

fn gate_has_positive_matcher(gate: &ManifestGate) -> bool {
    !gate.contains.is_empty()
        || !gate.regex.is_empty()
        || !gate.line_regex.is_empty()
        || !gate.all.is_empty()
        || !gate.any.is_empty()
}

fn gate_has_any_matcher(gate: &ManifestGate) -> bool {
    gate_has_positive_matcher(gate) || !gate.not_gate.is_empty()
}

/// Whatever `resolve_region` can dispatch is valid — probing it with an
/// empty input keeps the validator and the dispatcher from drifting apart.
fn validate_region_name(spec: &str) -> Result<(), String> {
    let probe = DetectionInput {
        screen: "",
        osc_title: "",
        osc_progress: "",
    };
    match resolve_region(&probe, spec.trim()) {
        Some(_) => Ok(()),
        None => Err(spec.trim().to_string()),
    }
}

/// A rule's top-level matchers form a gate of their own; validation and
/// compilation both see the rule through this lens.
fn manifest_gate_from_rule(rule: &ManifestRule) -> ManifestGate {
    ManifestGate {
        all: rule.all.clone(),
        any: rule.any.clone(),
        not_gate: rule.not_gate.clone(),
        contains: rule.contains.clone(),
        regex: rule.regex.clone(),
        line_regex: rule.line_regex.clone(),
    }
}

/// Load bundled + override manifests. The second value counts files that
/// were rejected (unreadable, unparseable, or invalid) — the details are
/// already in the log by the time this returns.
fn load_all() -> (HashMap<String, CompiledManifest>, usize) {
    let mut map = HashMap::new();
    let mut rejected = 0usize;
    for text in BUNDLED_MANIFESTS {
        match compile_manifest(text, "bundled") {
            Some(compiled) => {
                map.insert(compiled.id.clone(), compiled);
            }
            None => rejected += 1,
        }
    }
    let dir = override_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(err) => {
                    log::error!(
                        "agent manifest {} could not be read: {err:#}",
                        path.display()
                    );
                    rejected += 1;
                    continue;
                }
            };
            match compile_manifest(&text, &path.display().to_string()) {
                Some(compiled) => {
                    log::info!("agent detection override loaded for {}", compiled.id);
                    map.insert(compiled.id.clone(), compiled);
                }
                None => rejected += 1,
            }
        }
    }
    (map, rejected)
}

fn ensure_loaded() {
    let loaded = MANIFESTS.read().is_some();
    if !loaded {
        let (map, _rejected) = load_all();
        let mut slot = MANIFESTS.write();
        if slot.is_none() {
            *slot = Some(map);
            MANIFEST_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
}

/// Load the manifests if they are not loaded yet. Called once at detector
/// startup so the evaluation path never does filesystem IO.
pub(crate) fn warm() {
    ensure_loaded();
}

/// Reload bundled + override files. Builds the replacement map before
/// swapping it in, so concurrent evaluations never observe an empty
/// window and the caller can run this on a worker thread. Returns how
/// many manifest files were rejected, so the UI that triggered the
/// reload can say so instead of reporting unconditional success.
pub(crate) fn reload_manifests() -> usize {
    let (map, rejected) = load_all();
    *MANIFESTS.write() = Some(map);
    MANIFEST_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    rejected
}

pub(crate) fn with_manifests<R>(f: impl FnOnce(&HashMap<String, CompiledManifest>) -> R) -> R {
    ensure_loaded();
    let guard = MANIFESTS.read();
    match guard.as_ref() {
        Some(map) => f(map),
        None => f(&HashMap::new()),
    }
}

/// Find the manifest id whose id or aliases match a process basename.
pub(crate) fn manifest_id_for_alias(name: &str) -> Option<String> {
    with_manifests(|map| {
        if map.contains_key(name) {
            return Some(name.to_string());
        }
        // Deterministic across runs: HashMap order is randomized, so an
        // alias claimed by two manifests must not flip between them.
        let mut ids: Vec<&String> = map
            .values()
            .filter(|m| m.aliases.iter().any(|a| a == name))
            .map(|m| &m.id)
            .collect();
        ids.sort();
        ids.first().map(|id| (*id).clone())
    })
}

/// Run one agent's rules over the snapshot. Highest priority match wins;
/// on a priority tie the earlier rule in the file wins (herdr semantics).
pub fn evaluate(agent_id: &str, input: &DetectionInput) -> ScreenVerdict {
    with_manifests(|map| {
        let Some(manifest) = map.get(agent_id) else {
            return ScreenVerdict::NoMatch;
        };
        let lower_screen = input.screen.to_lowercase();
        let mut matched: Option<&CompiledRule> = None;
        for rule in &manifest.rules {
            let region_text = region(input, &rule.region);
            // The lowercase haystack is only pre-computed for the whole
            // screen; sliced regions are small, so lowercasing them per
            // rule is cheap and keeps slicing byte-exact.
            let lower_region: std::borrow::Cow<str> = if rule.region == "whole_recent" {
                std::borrow::Cow::Borrowed(&lower_screen)
            } else {
                std::borrow::Cow::Owned(region_text.to_lowercase())
            };
            if !gate_matches(&rule.gate, region_text, &lower_region) {
                continue;
            }
            match matched {
                Some(previous) if previous.priority >= rule.priority => {}
                _ => matched = Some(rule),
            }
        }
        match matched {
            Some(rule) if rule.skip_state_update => ScreenVerdict::Freeze,
            Some(rule) => ScreenVerdict::State(rule.state.unwrap_or(AgentState::Unknown)),
            None => ScreenVerdict::NoMatch,
        }
    })
}

fn gate_matches(gate: &CompiledGate, text: &str, lower_text: &str) -> bool {
    if !gate
        .contains
        .iter()
        .all(|needle| lower_text.contains(needle))
    {
        return false;
    }
    if !gate.regex.iter().all(|regex| regex.is_match(text)) {
        return false;
    }
    if !gate
        .line_regex
        .iter()
        .all(|regex| text.lines().any(|line| regex.is_match(line)))
    {
        return false;
    }
    if !gate
        .all
        .iter()
        .all(|nested| gate_matches(nested, text, lower_text))
    {
        return false;
    }
    if !gate.any.is_empty()
        && !gate
            .any
            .iter()
            .any(|nested| gate_matches(nested, text, lower_text))
    {
        return false;
    }
    if gate
        .not_gate
        .iter()
        .any(|nested| gate_matches(nested, text, lower_text))
    {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// Regions. Behaviour matches herdr's region resolvers so their manifests
// keep their meaning.
// ---------------------------------------------------------------------------

fn region<'a>(input: &DetectionInput<'a>, spec: &str) -> &'a str {
    resolve_region(input, spec).unwrap_or_else(|| {
        // Unreachable for loaded manifests — validation rejects unknown
        // regions at load time — but stay fail-soft like herdr rather
        // than turning a future gap into an evaluation-time panic.
        log::debug!("unknown agent manifest region {spec:?}");
        ""
    })
}

/// `None` means the spec names no region this engine knows. The validator
/// probes this with an empty input, so "validated" and "dispatchable" are
/// the same predicate by construction. Trimmed here as well as at compile
/// time (herdr trims at dispatch): two reviews independently flagged how
/// easily those two sites could drift apart.
fn resolve_region<'a>(input: &DetectionInput<'a>, spec: &str) -> Option<&'a str> {
    let spec = spec.trim();
    match spec {
        "osc_title" => return Some(input.osc_title),
        "osc_progress" => return Some(input.osc_progress),
        _ => {}
    }
    let content = input.screen;
    Some(match spec {
        "whole_recent" => content,
        "after_last_prompt_marker" => after_last_prompt_marker(content),
        "before_current_prompt_marker" => before_current_prompt_marker(content),
        "whole_recent_without_current_prompt_marker" => {
            whole_recent_without_current_prompt_marker(content)
        }
        "current_prompt_block_marker" => current_prompt_block_marker(content).unwrap_or(""),
        "after_current_prompt_block_marker" => {
            after_current_prompt_block_marker(content).unwrap_or("")
        }
        "prompt_box_body" => prompt_box_body(content).unwrap_or(""),
        "above_prompt_box" => above_prompt_box(content),
        "last_non_empty_above_prompt_box" => last_non_empty_line(above_prompt_box(content)),
        "after_last_horizontal_rule" => after_last_horizontal_rule(content),
        _ => {
            if let Some(count) = region_count(spec, "bottom_non_empty_lines") {
                bottom_non_empty_lines(content, count)
            } else if let Some(count) = region_count(spec, "bottom_lines") {
                bottom_lines(content, count)
            } else if let Some(count) = top_region_count(spec) {
                top_non_empty_lines(content, count)
            } else {
                return None;
            }
        }
    })
}

/// Lenient count parser for the `bottom_*` regions, matching herdr's
/// `region_count`: any `usize` parses, `0` and leading zeros included —
/// the slicers degrade to "" safely. Only `top_non_empty_lines` gets the
/// strict form below.
fn region_count(spec: &str, name: &str) -> Option<usize> {
    let count = spec
        .strip_prefix(name)?
        .strip_prefix('(')?
        .strip_suffix(')')?;
    count.parse::<usize>().ok()
}

/// Strict parser for `top_non_empty_lines(N)`, matching herdr's
/// `top_region_count`: rejects 0, leading zeros, signs, and counts above
/// `u16::MAX`.
fn top_region_count(spec: &str) -> Option<usize> {
    let count = spec
        .strip_prefix("top_non_empty_lines")?
        .strip_prefix('(')?
        .strip_suffix(')')?;
    if count.is_empty() || count.starts_with('0') || !count.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    count
        .parse::<usize>()
        .ok()
        .filter(|c| *c <= u16::MAX as usize)
}

fn line_start_offset(content: &str, lines: &[&str], index: usize) -> usize {
    lines[..index.min(lines.len())]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>()
        .min(content.len())
}

fn slice_from_line_index<'a>(content: &'a str, lines: &[&str], index: usize) -> &'a str {
    &content[line_start_offset(content, lines, index)..]
}

fn bottom_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(count);
    slice_from_line_index(content, &lines, start)
}

fn bottom_non_empty_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(start_index) = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map(|(index, _)| index)
    else {
        return "";
    };
    slice_from_line_index(content, &lines, start_index)
}

fn top_non_empty_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(end_index) = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map(|(index, _)| index)
    else {
        return "";
    };
    let byte_offset = line_start_offset(content, &lines, end_index + 1);
    &content[..byte_offset]
}

/// Codex draws its prompt as a `›` line; the region is everything after the
/// last one. No marker on screen → the whole content, not "".
fn after_last_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(index) = lines.iter().rposition(|line| codex_prompt_line(line)) else {
        return content;
    };
    slice_from_line_index(content, &lines, index + 1)
}

/// Everything above the *current* prompt marker (see
/// [`current_codex_prompt_index`]). No current prompt → the whole content.
fn before_current_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(index) = current_codex_prompt_index(&lines) else {
        return content;
    };
    let end = line_start_offset(content, &lines, index);
    &content[..end]
}

/// A gate rather than a slice: "" while a current prompt marker is on
/// screen, the whole content otherwise.
fn whole_recent_without_current_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    if current_codex_prompt_index(&lines).is_some() {
        ""
    } else {
        content
    }
}

/// The last block-marker line above the current prompt, as a single line
/// without its trailing newline. `None` when there is no current prompt or
/// no marker above it.
fn current_prompt_block_marker(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let prompt_index = current_codex_prompt_index(&lines)?;
    lines[..prompt_index]
        .iter()
        .rev()
        .find(|line| codex_block_marker_line(line))
        .copied()
}

/// From that same block-marker line — inclusive — to the end of content.
fn after_current_prompt_block_marker(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let prompt_index = current_codex_prompt_index(&lines)?;
    let block_index = lines[..prompt_index]
        .iter()
        .rposition(|line| codex_block_marker_line(line))?;
    Some(slice_from_line_index(content, &lines, block_index))
}

/// The last `›` line, but only while it is the *live* prompt: a block
/// marker anywhere below it means the agent produced output after that
/// prompt, so it is scrollback, not the prompt the user is looking at.
fn current_codex_prompt_index(lines: &[&str]) -> Option<usize> {
    let prompt_index = lines.iter().rposition(|line| codex_prompt_line(line))?;
    if lines[prompt_index + 1..]
        .iter()
        .any(|line| codex_block_marker_line(line))
    {
        return None;
    }
    Some(prompt_index)
}

/// Deliberately untrimmed, matching herdr: an indented `›` is quoted
/// output, not the prompt.
fn codex_prompt_line(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

fn codex_block_marker_line(line: &str) -> bool {
    line.starts_with('•') || line.starts_with('■') || line.starts_with('✗') || line.starts_with('✓')
}

/// The body of the bottom prompt box: between the second-to-last and last
/// horizontal rules.
fn prompt_box_body(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let top = prompt_box_top_border_index(&lines)?;
    let start = line_start_offset(content, &lines, top + 1);
    let end_index = lines[top + 1..]
        .iter()
        .position(|line| is_horizontal_rule(line))
        .map(|relative| top + 1 + relative)
        .unwrap_or(lines.len());
    let end = line_start_offset(content, &lines, end_index);
    Some(&content[start..end.max(start)])
}

fn above_prompt_box(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(top) = prompt_box_top_border_index(&lines) else {
        return content;
    };
    let end = line_start_offset(content, &lines, top);
    &content[..end]
}

fn after_last_horizontal_rule(content: &str) -> &str {
    let mut last_rule_end = 0usize;
    let mut offset = 0usize;
    for line in content.lines() {
        let next_offset = offset + line.len() + 1;
        if is_horizontal_rule(line) {
            last_rule_end = next_offset.min(content.len());
        }
        offset = next_offset;
    }
    &content[last_rule_end..]
}

fn last_non_empty_line(content: &str) -> &str {
    content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
}

fn prompt_box_top_border_index(lines: &[&str]) -> Option<usize> {
    let mut border_count = 0;
    for index in (0..lines.len()).rev() {
        if is_horizontal_rule(lines[index]) {
            border_count += 1;
            if border_count == 2 {
                return Some(index);
            }
        }
    }
    None
}

/// A run of `─` counts as a horizontal rule when nothing follows it, or when
/// it is at least three characters long (borders often carry a suffix such
/// as a hint label).
fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let rule_chars = trimmed.chars().take_while(|&ch| ch == '─').count();
    if rule_chars == 0 {
        return false;
    }
    let rule_bytes = trimmed
        .char_indices()
        .nth(rule_chars)
        .map(|(index, _)| index)
        .unwrap_or(trimmed.len());
    let suffix = trimmed[rule_bytes..].trim_start();
    suffix.is_empty() || rule_chars >= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_manifests_all_compile() {
        for text in BUNDLED_MANIFESTS {
            let raw_file: ManifestFile = toml::from_str(text).expect("parses");
            assert!(
                raw_file.min_engine_version.unwrap_or(0) <= ENGINE_VERSION,
                "bundled manifest {} demands a newer engine",
                raw_file.id
            );
            validate_manifest(&raw_file)
                .unwrap_or_else(|err| panic!("bundled manifest {} is invalid: {err}", raw_file.id));
            let compiled = compile_manifest(text, "test").expect("manifest parses");
            assert!(!compiled.id.is_empty());
            assert!(!compiled.rules.is_empty(), "{} has rules", compiled.id);
            // Every rule must have compiled; compile_manifest drops broken
            // ones, so compare against a fresh parse of the raw file.
            let raw: toml::Value = toml::from_str(text).expect("raw toml");
            let raw_rules = raw
                .get("rules")
                .and_then(|r| r.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            assert_eq!(
                compiled.rules.len(),
                raw_rules,
                "{}: all rules compile",
                compiled.id
            );
        }
    }

    #[test]
    fn alias_lookup_matches_id_and_alias() {
        assert_eq!(manifest_id_for_alias("claude").as_deref(), Some("claude"));
        assert_eq!(
            manifest_id_for_alias("claude-code").as_deref(),
            Some("claude")
        );
        assert_eq!(manifest_id_for_alias("definitely-not-an-agent"), None);
    }

    fn eval_claude(screen: &str, title: &str, progress: &str) -> ScreenVerdict {
        evaluate(
            "claude",
            &DetectionInput {
                screen,
                osc_title: title,
                osc_progress: progress,
            },
        )
    }

    #[test]
    fn claude_title_spinner_is_working() {
        let verdict = eval_claude("", "◐ fix the tests", "");
        assert!(matches!(verdict, ScreenVerdict::State(AgentState::Working)));
        let verdict = eval_claude("", "⠋ fix the tests", "");
        assert!(matches!(verdict, ScreenVerdict::State(AgentState::Working)));
    }

    #[test]
    fn claude_esc_to_interrupt_is_working() {
        let screen = "some output\n⏵⏵ Cooking… (esc to interrupt · 12s)\n";
        let verdict = eval_claude(screen, "", "");
        assert!(matches!(verdict, ScreenVerdict::State(AgentState::Working)));
    }

    #[test]
    fn claude_permission_prompt_is_blocked() {
        let screen = "\
Bash command

──────────────────────────────
Do you want to proceed?
❯ 1. Yes
  2. No
Esc to cancel
";
        let verdict = eval_claude(screen, "", "");
        assert!(matches!(verdict, ScreenVerdict::State(AgentState::Blocked)));
    }

    #[test]
    fn claude_prompt_box_is_idle() {
        let screen = "\
previous output
──────────────────────────────
❯
──────────────────────────────
  ? for shortcuts
";
        let verdict = eval_claude(screen, "", "");
        assert!(matches!(verdict, ScreenVerdict::State(AgentState::Idle)));
    }

    #[test]
    fn claude_transcript_viewer_freezes() {
        let screen = "\
transcript line
Showing detailed transcript
ctrl+o to toggle
";
        let verdict = eval_claude(screen, "", "");
        assert!(matches!(verdict, ScreenVerdict::Freeze));
    }

    #[test]
    fn empty_screen_no_signals_is_progress_idle() {
        // osc_progress "4;0" hits claude's osc_progress_idle rule.
        let verdict = eval_claude("", "", "4;0");
        assert!(matches!(verdict, ScreenVerdict::State(AgentState::Idle)));
    }

    #[test]
    fn shell_prompt_without_box_is_no_match() {
        let verdict = eval_claude("~/src\n❯ \n", "", "");
        assert!(matches!(verdict, ScreenVerdict::NoMatch));
    }

    #[test]
    fn codex_action_required_title_is_blocked() {
        let verdict = evaluate(
            "codex",
            &DetectionInput {
                screen: "",
                osc_title: "Action Required - approve the command",
                osc_progress: "",
            },
        );
        assert!(matches!(verdict, ScreenVerdict::State(AgentState::Blocked)));
    }

    #[test]
    fn regions_slice_as_documented() {
        let content = "a\n\nb\nc\n";
        assert_eq!(bottom_non_empty_lines(content, 2), "b\nc\n");
        assert_eq!(top_non_empty_lines(content, 1), "a\n");
        assert_eq!(bottom_lines(content, 1), "c\n");
        let boxed = "x\n───\nbody\n───\n";
        assert_eq!(prompt_box_body(boxed), Some("body\n"));
        assert_eq!(above_prompt_box(boxed), "x\n");
        assert_eq!(after_last_horizontal_rule(boxed), "");
        assert_eq!(last_non_empty_line("a\nb\n\n"), "b");
        assert!(is_horizontal_rule("──────"));
        assert!(is_horizontal_rule("─── hint"));
        assert!(!is_horizontal_rule("- - -"));
    }

    #[test]
    fn codex_marker_regions_slice_as_documented() {
        // Only an unindented "›" (bare, or followed by a space) is the
        // prompt; anything else is quoted output.
        assert!(codex_prompt_line("›"));
        assert!(codex_prompt_line("› run tests"));
        assert!(!codex_prompt_line("›run"));
        assert!(!codex_prompt_line("  › run"));

        // A live prompt with a finished block above it.
        let live = "• first\n✗ second\n› \ntail\n";
        assert_eq!(before_current_prompt_marker(live), "• first\n✗ second\n");
        assert_eq!(whole_recent_without_current_prompt_marker(live), "");
        assert_eq!(current_prompt_block_marker(live), Some("✗ second"));
        assert_eq!(
            after_current_prompt_block_marker(live),
            Some("✗ second\n› \ntail\n")
        );

        // A block marker below the prompt means the prompt is scrollback:
        // the "current prompt" family must treat the screen as promptless.
        let stale = "› run\n• working\n";
        assert_eq!(before_current_prompt_marker(stale), stale);
        assert_eq!(whole_recent_without_current_prompt_marker(stale), stale);
        assert_eq!(current_prompt_block_marker(stale), None);
        assert_eq!(after_current_prompt_block_marker(stale), None);

        // No prompt at all.
        assert_eq!(before_current_prompt_marker("hello\n"), "hello\n");
        assert_eq!(whole_recent_without_current_prompt_marker("hello\n"), "hello\n");
        assert_eq!(after_last_prompt_marker("hello\n"), "hello\n");
        assert_eq!(after_last_prompt_marker("› go\nout\n"), "out\n");

        // All five are reachable as region specs, not just as helpers.
        let input = DetectionInput {
            screen: live,
            osc_title: "",
            osc_progress: "",
        };
        assert_eq!(region(&input, "before_current_prompt_marker"), "• first\n✗ second\n");
        assert_eq!(region(&input, "whole_recent_without_current_prompt_marker"), "");
        assert_eq!(region(&input, "current_prompt_block_marker"), "✗ second");
        assert_eq!(
            region(&input, "after_current_prompt_block_marker"),
            "✗ second\n› \ntail\n"
        );
        assert_eq!(region(&input, "above_prompt_box"), live);
    }

    fn validate_error(text: &str) -> String {
        let file: ManifestFile = toml::from_str(text).expect("manifest must parse");
        validate_manifest(&file).expect_err("manifest must be rejected")
    }

    #[test]
    fn unknown_manifest_fields_are_rejected() {
        // "contain" for "contains" — the classic typo, caught by serde.
        let text = "id = \"t\"\n[[rules]]\nid = \"r\"\nstate = \"working\"\ncontain = [\"x\"]\n";
        let err = match toml::from_str::<ManifestFile>(text) {
            Ok(_) => panic!("the unknown field must be rejected"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("contain"), "{err}");
        assert!(compile_manifest(text, "test").is_none());
    }

    /// The file level stays open like herdr's: a future manifest revision
    /// adding top-level metadata must still drop in unchanged.
    #[test]
    fn unknown_top_level_keys_are_tolerated() {
        let text = "id = \"t\"\ndescription = \"future metadata\"\n\
                    [[rules]]\nid = \"r\"\nstate = \"working\"\ncontains = [\"x\"]\n";
        let compiled = compile_manifest(text, "test").expect("loads despite the unknown key");
        assert_eq!(compiled.rules.len(), 1);
    }

    #[test]
    fn invalid_manifests_are_rejected_with_reasons() {
        assert!(validate_error("id = \"t\"\n").contains("at least one rule"));
        assert!(validate_error(
            "id = \"t\"\n[[rules]]\nid = \"r\"\nstate = \"idle\"\nregion = \"after_last_promt_marker\"\ncontains = [\"x\"]\n"
        )
        .contains("uses invalid region"));
        assert!(validate_error(
            "id = \"t\"\n[[rules]]\nid = \" \"\nstate = \"idle\"\ncontains = [\"x\"]\n"
        )
        .contains("rule id must not be empty"));
        assert!(validate_error(
            "id = \"t\"\n[[rules]]\nid = \"r\"\nstate = \"working\"\nskip_state_update = true\ncontains = [\"x\"]\n"
        )
        .contains("skip_state_update without state"));
        assert!(validate_error(
            "id = \"t\"\n[[rules]]\nid = \"r\"\nstate = \"unknown\"\nskip_state_update = true\nvisible_working = true\ncontains = [\"x\"]\n"
        )
        .contains("visible state evidence"));
        // A rule whose only matcher is a `not` gate matches everything the
        // gate doesn't name — reject it like herdr does.
        assert!(validate_error(
            "id = \"t\"\n[[rules]]\nid = \"r\"\nstate = \"idle\"\nnot = [{ contains = [\"x\"] }]\n"
        )
        .contains("must contain a positive matcher"));
        assert!(validate_error(
            "id = \"t\"\n[[rules]]\nid = \"r\"\nstate = \"idle\"\nregex = [\"[\"]\n"
        )
        .contains("invalid regex pattern"));
        assert!(validate_error(
            "id = \"t\"\n[[rules]]\nid = \"r\"\nstate = \"idle\"\nany = [{ line_regex = [\"[\"] }]\n"
        )
        .contains("invalid line_regex pattern"));
        assert!(validate_error(
            "id = \"t\"\nmin_engine_version = 1\n[[rules]]\nid = \"r\"\nstate = \"idle\"\nregion = \"top_non_empty_lines(1)\"\ncontains = [\"x\"]\n"
        )
        .contains("min_engine_version is below"));
        let long = "x".repeat(MAX_MATCHER_CHARS + 1);
        assert!(
            validate_error(&format!(
                "id = \"t\"\n[[rules]]\nid = \"r\"\nstate = \"idle\"\ncontains = [\"{long}\"]\n"
            ))
            .contains("max length")
        );
        let mut many_rules = String::from("id = \"t\"\n");
        for i in 0..=MAX_RULES_PER_MANIFEST {
            many_rules.push_str(&format!(
                "[[rules]]\nid = \"r{i}\"\nstate = \"idle\"\ncontains = [\"x\"]\n"
            ));
        }
        assert!(validate_error(&many_rules).contains("max is 128"));
    }

    fn nested_all_manifest(extra_depth: usize) -> String {
        let mut gate = String::from("{ contains = [\"x\"] }");
        for _ in 0..extra_depth {
            gate = format!("{{ all = [{gate}] }}");
        }
        format!(
            "id = \"t\"\n[[rules]]\nid = \"r\"\nstate = \"idle\"\ncontains = [\"x\"]\nall = [{gate}]\n"
        )
    }

    #[test]
    fn gate_depth_limit_binds_at_the_documented_boundary() {
        // The rule's own gate sits at depth 0 and the check is `>`, so the
        // innermost inline gate may sit at depth 8 but not 9.
        let file: ManifestFile = toml::from_str(&nested_all_manifest(7)).expect("parses");
        validate_manifest(&file).expect("depth 8 is within the limit");
        assert!(validate_error(&nested_all_manifest(8)).contains("max gate depth"));
    }

    #[test]
    fn padded_and_versioned_regions_validate() {
        assert!(validate_region_name(" top_non_empty_lines(1) ").is_ok());
        assert!(validate_region_name("bottom_non_empty_lines(12)").is_ok());
        // The bottom_* parsers are deliberately lenient like herdr's:
        // degenerate counts slice to "" rather than rejecting a manifest.
        assert!(validate_region_name("bottom_lines(0)").is_ok());
        assert!(validate_region_name("bottom_non_empty_lines(70000)").is_ok());
        assert!(validate_region_name("bottom_non_empty_lines(007)").is_ok());
        // Only top_non_empty_lines is strict (herdr's top_region_count).
        assert!(validate_region_name("top_non_empty_lines(0)").is_err());
        assert!(validate_region_name("top_non_empty_lines(01)").is_err());
        assert!(validate_region_name("top_non_empty_lines(65536)").is_err());
        assert!(validate_region_name("after_last_promt_marker").is_err());
        // The dispatcher must accept exactly what the validator accepts,
        // padding included.
        let input = DetectionInput {
            screen: "a\nb\n",
            osc_title: "",
            osc_progress: "",
        };
        assert_eq!(
            region(&input, " top_non_empty_lines(1) "),
            region(&input, "top_non_empty_lines(1)")
        );
        for name in [
            "whole_recent",
            "after_last_prompt_marker",
            "before_current_prompt_marker",
            "whole_recent_without_current_prompt_marker",
            "current_prompt_block_marker",
            "after_current_prompt_block_marker",
            "prompt_box_body",
            "above_prompt_box",
            "last_non_empty_above_prompt_box",
            "after_last_horizontal_rule",
            "osc_title",
            "osc_progress",
        ] {
            assert!(validate_region_name(name).is_ok(), "{name} must validate");
        }
    }
}
