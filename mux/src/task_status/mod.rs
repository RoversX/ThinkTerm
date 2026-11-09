//! What a long-running command-line tool -- a package manager, a build --
//! is doing, read off its screen by rules, as agent status is
//! (`crate::agent_status`): one TOML file per tool, bundled here and
//! overridable from `~/.config/thinkterm/task-detection/`.
//!
//! A tool's file names the programs it is about and lists rules. Each rule
//! looks at a region of the screen's bottom with line regexes; the first
//! line (from the bottom) one of them matches makes the rule match. Named
//! groups take the numbers out: `percent`, `done` and `total`, and any other
//! group can be put into the rule's `detail` text as `{name}`. Rules are
//! weighed by priority: the state comes from the highest matching rule
//! that sets one, and each of percent, count and detail from the highest
//! matching rule that has it. See `docs/thinkterm/task-status.md`.
//!
//! Only the reading is decided here. When to read, and what to show of it,
//! is the frontend's business.

use parking_lot::RwLock;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use thinkterm_proto::program::{is_hidden_char, MAX_TASK_DETAIL_CHARS, MAX_TASK_NAME_BYTES};
pub use thinkterm_proto::{TaskReading, TaskState};

/// One tool's rules, compiled.
pub struct TaskRules {
    pub id: String,
    /// What to call the tool.
    pub name: String,
    programs: Vec<String>,
    rules: Vec<Rule>,
}

struct Rule {
    state: Option<TaskState>,
    priority: i32,
    region: Region,
    line_regex: Vec<Regex>,
    detail: Option<String>,
}

#[derive(Clone, Copy)]
enum Region {
    /// The last `n` lines that are not blank.
    BottomNonEmptyLines(usize),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFile {
    id: String,
    #[serde(default)]
    name: Option<String>,
    programs: Vec<String>,
    #[serde(default)]
    rules: Vec<RuleFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    priority: i32,
    region: String,
    line_regex: Vec<String>,
    #[serde(default)]
    detail: Option<String>,
}

/// The tools ThinkTerm knows out of the box.
const BUNDLED: &[&str] = &[include_str!("rules/apt.toml"), include_str!("rules/brew.toml")];

/// Lines a region may cover at most: rules look at the screen's bottom.
const MAX_REGION_LINES: usize = 40;

static RULES: RwLock<Option<HashMap<String, Arc<TaskRules>>>> = RwLock::new(None);

/// The rules for a program, by the name ThinkTerm sees it under (`apt`,
/// `brew.rb`), if any tool claims it.
pub fn rules_for(program: &str) -> Option<Arc<TaskRules>> {
    if RULES.read().is_none() {
        reload();
    }
    RULES
        .read()
        .as_ref()?
        .values()
        .find(|rules| rules.programs.iter().any(|name| name == program))
        .cloned()
}

/// Load the bundled rules and the user's: a `<id>.toml` in the override
/// directory replaces the bundled file with that id, and any other is added.
pub fn reload() {
    let mut map = HashMap::new();
    for text in BUNDLED {
        if let Some(rules) = compile(text, "bundled") {
            map.insert(rules.id.clone(), Arc::new(rules));
        }
    }
    if let Ok(entries) = std::fs::read_dir(override_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(rules) = compile(&text, &path.display().to_string()) {
                map.insert(rules.id.clone(), Arc::new(rules));
            }
        }
    }
    *RULES.write() = Some(map);
}

fn override_dir() -> std::path::PathBuf {
    // As with agent rules, tests must not read the developer's own.
    #[cfg(test)]
    {
        return std::path::PathBuf::from("/nonexistent-thinkterm-test-task-detection");
    }
    #[cfg(not(test))]
    config::HOME_DIR
        .join(".config")
        .join("thinkterm")
        .join("task-detection")
}

fn compile(text: &str, origin: &str) -> Option<TaskRules> {
    let file: TaskFile = match toml::from_str(text) {
        Ok(file) => file,
        Err(err) => {
            log::error!("task rules {origin} failed to parse: {err:#}");
            return None;
        }
    };
    let mut rules = Vec::new();
    for rule in file.rules {
        let state = match rule.state.as_deref() {
            None => None,
            Some("running") => Some(TaskState::Running),
            Some("waiting") => Some(TaskState::Waiting),
            Some("error") => Some(TaskState::Error),
            Some(other) => {
                log::error!("task rules {origin}: unknown state {other:?}");
                return None;
            }
        };
        let Some(region) = parse_region(&rule.region) else {
            log::error!("task rules {origin}: unknown region {:?}", rule.region);
            return None;
        };
        let mut line_regex = Vec::new();
        for pattern in &rule.line_regex {
            match Regex::new(pattern) {
                Ok(regex) => line_regex.push(regex),
                Err(err) => {
                    log::error!("task rules {origin}: bad regex {pattern:?}: {err}");
                    return None;
                }
            }
        }
        rules.push(Rule {
            state,
            priority: rule.priority,
            region,
            line_regex,
            detail: rule.detail,
        });
    }
    rules.sort_by_key(|rule| std::cmp::Reverse(rule.priority));
    let name = match file.name {
        Some(name) => name,
        None => file.id.clone(),
    };
    if name.is_empty() || name.len() > MAX_TASK_NAME_BYTES || name.chars().any(is_hidden_char) {
        log::error!("task rules {origin}: the name must be 1 to {MAX_TASK_NAME_BYTES} bytes");
        return None;
    }
    Some(TaskRules {
        id: file.id,
        name,
        programs: file.programs,
        rules,
    })
}

fn parse_region(text: &str) -> Option<Region> {
    let count = text
        .strip_prefix("bottom_non_empty_lines(")?
        .strip_suffix(')')?
        .trim()
        .parse::<usize>()
        .ok()?;
    (1..=MAX_REGION_LINES)
        .contains(&count)
        .then_some(Region::BottomNonEmptyLines(count))
}

impl TaskRules {
    /// Read `screen`, its lines top to bottom.
    pub fn read(&self, screen: &[String]) -> TaskReading {
        let mut reading = TaskReading {
            name: self.name.clone(),
            ..TaskReading::default()
        };
        let mut state = None;
        let mut detail = None;
        for rule in &self.rules {
            let Some(captures) = rule.find(screen) else {
                continue;
            };
            if state.is_none() {
                state = rule.state;
            }
            if reading.percent.is_none() {
                reading.percent = captures
                    .get("percent")
                    .and_then(|text| text.split('.').next()?.parse::<u8>().ok())
                    .map(|percent| percent.min(100));
            }
            if reading.count.is_none() {
                let number = |name| captures.get(name).and_then(|text| text.parse::<u32>().ok());
                if let (Some(done), Some(total)) = (number("done"), number("total")) {
                    if total > 0 {
                        reading.count = Some((done.min(total), total));
                    }
                }
            }
            if detail.is_none() {
                detail = match &rule.detail {
                    Some(template) => Some(fill(template, &captures)),
                    None => captures.get("detail").cloned(),
                };
            }
        }
        reading.state = state.unwrap_or_default();
        reading.detail = detail
            .map(|text| {
                text.chars()
                    .filter(|c| !is_hidden_char(*c))
                    .collect::<String>()
                    .trim()
                    .chars()
                    .take(MAX_TASK_DETAIL_CHARS)
                    .collect::<String>()
            })
            .filter(|text| !text.is_empty());
        reading
    }
}

impl Rule {
    /// The named groups of the first line from the bottom of the region
    /// that one of the rule's regexes matches.
    fn find(&self, screen: &[String]) -> Option<HashMap<String, String>> {
        let Region::BottomNonEmptyLines(count) = self.region;
        let lines = screen
            .iter()
            .rev()
            .map(|line| line.trim_end())
            .filter(|line| !line.trim().is_empty())
            .take(count);
        for line in lines {
            for regex in &self.line_regex {
                let Some(found) = regex.captures(line) else {
                    continue;
                };
                return Some(
                    regex
                        .capture_names()
                        .flatten()
                        .filter_map(|name| {
                            Some((name.to_string(), found.name(name)?.as_str().to_string()))
                        })
                        .collect(),
                );
            }
        }
        None
    }
}

/// `template` with each `{name}` replaced by what that group took.
fn fill(template: &str, captures: &HashMap<String, String>) -> String {
    let mut text = template.to_string();
    for (name, value) in captures {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

/// The bottom of a pane's screen, as the rules read it: the visible rows,
/// whatever the user has scrolled to.
pub fn screen_tail(pane: &dyn crate::pane::Pane) -> Vec<String> {
    let dims = pane.get_dimensions();
    if dims.cols == 0 || dims.viewport_rows == 0 {
        return Vec::new();
    }
    let last = dims.physical_top + dims.viewport_rows as isize;
    let start = (last - MAX_REGION_LINES as isize).max(dims.physical_top);
    let (_first, lines) = pane.get_lines(start..last);
    lines.iter().map(|line| line.as_str().to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    fn bundled(program: &str) -> Arc<TaskRules> {
        reload();
        rules_for(program).expect("bundled rules")
    }

    #[test]
    fn the_bundled_rules_compile_and_claim_their_programs() {
        assert_eq!(bundled("apt").id, "apt");
        assert_eq!(bundled("apt-get").id, "apt");
        assert_eq!(bundled("brew").id, "brew");
        assert_eq!(bundled("brew.rb").name, "brew");
        assert!(rules_for("vim").is_none());
    }

    #[test]
    fn apt_downloads_then_installs_with_its_progress_line() {
        let apt = bundled("apt");
        let reading = apt.read(&screen(
            "Get:3 http://archive.ubuntu.com/ubuntu jammy-updates/main amd64 libssl3 amd64 3.0.2 [1,903 kB]\n\
             45% [3 libssl3 1,234 kB/1,903 kB 65%]",
        ));
        assert_eq!(reading.state, TaskState::Running);
        assert_eq!(reading.percent, Some(45));
        assert_eq!(reading.detail.as_deref(), Some("Downloading libssl3"));

        let reading = apt.read(&screen(
            "Unpacking libssl3:amd64 (3.0.2-0ubuntu1.15) over (3.0.2-0ubuntu1.14) ...\n\
             Setting up libssl3:amd64 (3.0.2-0ubuntu1.15) ...\n\
             \n\
             Progress: [ 62%] [#####################################.......................]",
        ));
        assert_eq!(reading.percent, Some(62));
        assert_eq!(reading.detail.as_deref(), Some("Setting up libssl3:amd64"));
    }

    #[test]
    fn apt_asking_to_go_on_is_waiting_and_an_e_line_is_an_error() {
        let apt = bundled("apt");
        let reading = apt.read(&screen(
            "After this operation, 12.3 MB of additional disk space will be used.\n\
             Do you want to continue? [Y/n] ",
        ));
        assert_eq!(reading.state, TaskState::Waiting);
        // In another language the question reads differently; its choices
        // do not.
        let reading = apt.read(&screen("您希望继续执行吗？ [Y/n] "));
        assert_eq!(reading.state, TaskState::Waiting);
        let reading = apt.read(&screen("E: Unable to locate package nosuchthing"));
        assert_eq!(reading.state, TaskState::Error);
    }

    #[test]
    fn brew_names_what_it_upgrades_and_how_far_a_download_is() {
        let brew = bundled("brew");
        let reading = brew.read(&screen(
            "==> Upgrading 2 outdated packages:\n\
             node 20.1.0 -> 21.0.0\n\
             ==> Fetching node\n\
             ==> Downloading https://ghcr.io/v2/homebrew/core/node/blobs/sha256:0123abcd\n\
             ##########################                                                 36.4%",
        ));
        assert_eq!(reading.percent, Some(36));
        assert_eq!(reading.detail.as_deref(), Some("Downloading node"));

        let reading = brew.read(&screen(
            "==> Upgrading node\n  20.1.0 -> 21.0.0\n\n==> Pouring node--21.0.0.arm64_sonoma.bottle.tar.gz",
        ));
        assert_eq!(reading.percent, None);
        assert_eq!(reading.detail.as_deref(), Some("Installing node"));

        let reading = brew.read(&screen("Error: node: no bottle available!"));
        assert_eq!(reading.state, TaskState::Error);
    }

    #[test]
    fn a_rule_takes_counts_and_fills_its_detail() {
        let rules = compile(
            r#"
            id = "demo"
            programs = ["demo"]
            [[rules]]
            id = "step"
            region = "bottom_non_empty_lines(2)"
            line_regex = ['^\((?P<done>\d+)/(?P<total>\d+)\) installing (?P<pkg>\S+)']
            detail = "Installing {pkg}"
            "#,
            "test",
        )
        .unwrap();
        let reading = rules.read(&screen("(3/12) installing ripgrep"));
        assert_eq!(reading.count, Some((3, 12)));
        assert_eq!(reading.detail.as_deref(), Some("Installing ripgrep"));
    }

    #[test]
    fn what_cannot_be_seen_is_taken_out_of_a_detail() {
        let apt = bundled("apt");
        let reading = apt.read(&screen("Setting up lib\u{202E}foo:amd64 (1.0) ..."));
        assert_eq!(reading.detail.as_deref(), Some("Setting up libfoo:amd64"));
    }

    #[test]
    fn a_file_with_a_mistake_is_left_out() {
        let bad_state = r#"
            id = "x"
            programs = ["x"]
            [[rules]]
            id = "r"
            state = "sleeping"
            region = "bottom_non_empty_lines(1)"
            line_regex = ['x']
        "#;
        assert!(compile(bad_state, "test").is_none());
        let bad_region = bad_state.replace("state = \"sleeping\"\n", "").replace("(1)", "(999)");
        assert!(compile(&bad_region, "test").is_none());
    }
}
