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

#[derive(Deserialize, Clone, Copy)]
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
    let mut rules = Vec::with_capacity(file.rules.len());
    for rule in &file.rules {
        let gate = match compile_gate(&ManifestGate {
            all: rule.all.clone(),
            any: rule.any.clone(),
            not_gate: rule.not_gate.clone(),
            contains: rule.contains.clone(),
            regex: rule.regex.clone(),
            line_regex: rule.line_regex.clone(),
        }) {
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

fn load_all() -> HashMap<String, CompiledManifest> {
    let mut map = HashMap::new();
    for text in BUNDLED_MANIFESTS {
        if let Some(compiled) = compile_manifest(text, "bundled") {
            map.insert(compiled.id.clone(), compiled);
        }
    }
    let dir = override_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(compiled) = compile_manifest(&text, &path.display().to_string()) {
                log::info!("agent detection override loaded for {}", compiled.id);
                map.insert(compiled.id.clone(), compiled);
            }
        }
    }
    map
}

fn ensure_loaded() {
    let loaded = MANIFESTS.read().is_some();
    if !loaded {
        let map = load_all();
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
/// window and the caller can run this on a worker thread.
pub(crate) fn reload_manifests() {
    let map = load_all();
    *MANIFESTS.write() = Some(map);
    MANIFEST_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
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
    match spec {
        "osc_title" => return input.osc_title,
        "osc_progress" => return input.osc_progress,
        _ => {}
    }
    let content = input.screen;
    match spec {
        "whole_recent" => content,
        "after_last_prompt_marker" => after_last_prompt_marker(content),
        "prompt_box_body" => prompt_box_body(content).unwrap_or(""),
        "last_non_empty_above_prompt_box" => last_non_empty_line(above_prompt_box(content)),
        "after_last_horizontal_rule" => after_last_horizontal_rule(content),
        _ => {
            if let Some(count) = region_count(spec, "bottom_non_empty_lines") {
                bottom_non_empty_lines(content, count)
            } else if let Some(count) = region_count(spec, "bottom_lines") {
                bottom_lines(content, count)
            } else if let Some(count) = region_count(spec, "top_non_empty_lines") {
                top_non_empty_lines(content, count)
            } else {
                log::warn!("unknown agent manifest region {spec:?}");
                ""
            }
        }
    }
}

fn region_count(spec: &str, name: &str) -> Option<usize> {
    let count = spec
        .strip_prefix(name)?
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
/// last one.
fn after_last_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(index) = lines
        .iter()
        .rposition(|line| *line == "›" || line.starts_with("› "))
    else {
        return content;
    };
    slice_from_line_index(content, &lines, index + 1)
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
}
