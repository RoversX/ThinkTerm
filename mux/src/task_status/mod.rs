//! What a long-running command-line tool -- a package manager, a build --
//! is doing, read off its screen by rules, as agent status is
//! (`crate::agent_status`): one TOML file per tool, bundled here and
//! overridable from `~/.config/thinkterm/task-detection/`.
//!
//! A tool's file names the programs it is about and lists rules. Each rule
//! looks at a region of the screen's bottom with line regexes; the first
//! line (from the bottom) one of them matches makes the rule match. Named
//! groups take the numbers out: `percent` (or `done_size` and `total_size`,
//! sizes such as "10.0MB", for it), `done` and `total`, and any other
//! group can be put into the rule's `detail` text as `{name}`. A rule with
//! `sum` counts every line it matches rather than the bottom one: their
//! sizes and counts add up, for as long as any of them is under way. A rule
//! with `after_regex` looks no higher than the last line that matches it --
//! the heading of what the tool is doing now. Rules are
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
    after_regex: Vec<Regex>,
    detail: Option<String>,
    sum: bool,
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
    after_regex: Vec<String>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    sum: bool,
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
        let compile_all = |patterns: &[String]| -> Option<Vec<Regex>> {
            let mut compiled = Vec::new();
            for pattern in patterns {
                match Regex::new(pattern) {
                    Ok(regex) => compiled.push(regex),
                    Err(err) => {
                        log::error!("task rules {origin}: bad regex {pattern:?}: {err}");
                        return None;
                    }
                }
            }
            Some(compiled)
        };
        let line_regex = compile_all(&rule.line_regex)?;
        let after_regex = compile_all(&rule.after_regex)?;
        rules.push(Rule {
            state,
            priority: rule.priority,
            region,
            line_regex,
            after_regex,
            detail: rule.detail,
            sum: rule.sum,
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
                    .map(|percent| percent.min(100))
                    .or_else(|| {
                        let done = parse_size(captures.get("done_size")?)?;
                        let total = parse_size(captures.get("total_size")?)?;
                        (total > 0.0).then(|| (done / total * 100.0).floor().clamp(0.0, 100.0) as u8)
                    });
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
    /// What the rule takes from `screen`: the named groups of the first
    /// line from the bottom of the region that one of its regexes matches
    /// -- or, with `sum`, of all of them added up.
    fn find(&self, screen: &[String]) -> Option<HashMap<String, String>> {
        if self.sum {
            self.find_sum(screen)
        } else {
            self.matches(screen).next()
        }
    }

    /// The named groups of each line of the region, from the bottom up,
    /// that one of the rule's regexes matches -- up to the first line an
    /// `after_regex` matches, which starts what the tool is doing now.
    fn matches<'a>(&'a self, screen: &'a [String]) -> impl Iterator<Item = HashMap<String, String>> + 'a {
        let Region::BottomNonEmptyLines(count) = self.region;
        screen
            .iter()
            .rev()
            .map(|line| line.trim_end())
            .filter(|line| !line.trim().is_empty())
            .take(count)
            .take_while(move |line| !self.after_regex.iter().any(|regex| regex.is_match(line)))
            .filter_map(move |line| {
                self.line_regex.iter().find_map(|regex| {
                    let found = regex.captures(line)?;
                    Some(
                        regex
                            .capture_names()
                            .flatten()
                            .filter_map(|name| {
                                Some((name.to_string(), found.name(name)?.as_str().to_string()))
                            })
                            .collect(),
                    )
                })
            })
    }

    /// Every line the rule matches, while any is still under way: the
    /// bottom one's groups, with `done_size` and `total_size` (and `done`
    /// and `total`) added up over the lines that have both, `lines` how
    /// many there are and `lines_done` how many have got to their total.
    fn find_sum(&self, screen: &[String]) -> Option<HashMap<String, String>> {
        let mut bottom = None;
        let (mut lines, mut lines_done) = (0u64, 0u64);
        let mut sizes: Option<(f64, f64)> = None;
        let mut counts: Option<(u64, u64)> = None;
        for groups in self.matches(screen) {
            let size = |name| groups.get(name).and_then(|text| parse_size(text));
            let count = |name| groups.get(name).and_then(|text| text.parse::<u64>().ok());
            let size = size("done_size").zip(size("total_size"));
            let count = count("done").zip(count("total"));
            lines += 1;
            let complete = match (size, count) {
                (Some((done, total)), _) => done >= total,
                (None, Some((done, total))) => done >= total,
                (None, None) => false,
            };
            if complete {
                lines_done += 1;
            }
            if let Some((done, total)) = size {
                let sum = sizes.get_or_insert((0.0, 0.0));
                sum.0 += done;
                sum.1 += total;
            }
            if let Some((done, total)) = count {
                let sum = counts.get_or_insert((0, 0));
                sum.0 = sum.0.saturating_add(done);
                sum.1 = sum.1.saturating_add(total);
            }
            bottom.get_or_insert(groups);
        }
        // Nothing, or all of it done: the tool has moved on.
        if lines == lines_done {
            return None;
        }
        let mut groups: HashMap<String, String> = bottom?;
        for name in ["done_size", "total_size", "done", "total"] {
            groups.remove(name);
        }
        if let Some((done, total)) = sizes {
            groups.insert("done_size".into(), format!("{done}"));
            groups.insert("total_size".into(), format!("{total}"));
        }
        if let Some((done, total)) = counts {
            groups.insert("done".into(), done.to_string());
            groups.insert("total".into(), total.to_string());
        }
        groups.insert("lines".into(), lines.to_string());
        groups.insert("lines_done".into(), lines_done.to_string());
        Some(groups)
    }
}

/// A size as tools print it -- "10.0MB", "512 B", "1.2 GiB" -- in bytes.
fn parse_size(text: &str) -> Option<f64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    let scale = match unit.trim() {
        "" | "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(number * scale)
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

        // An index has no package name, only its size where one would be.
        let reading = apt.read(&screen(
            "Get:2 http://archive.example/ubuntu jammy-updates InRelease [128 kB]\n\
             12% [Waiting for headers]",
        ));
        assert_eq!(reading.percent, Some(12));
        assert_eq!(reading.detail, None);

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
        // Some languages translate the choices too.
        for question in ["Möchten Sie fortfahren? [J/n] ", "Хотите продолжить? [Д/н] ", "Continuer ? [o/N] "] {
            assert_eq!(apt.read(&screen(question)).state, TaskState::Waiting, "{}", question);
        }
        let reading = apt.read(&screen("E: Unable to locate package nosuchthing"));
        assert_eq!(reading.state, TaskState::Error);
    }

    #[test]
    fn a_password_or_a_changed_config_file_waits_for_the_user() {
        let apt = bundled("apt");
        for prompt in [
            "[sudo] password for user: ",
            "[sudo: authenticate] Password: ",
            "[sudo] user 的密码：",
            "Password:",
        ] {
            assert_eq!(apt.read(&screen(prompt)).state, TaskState::Waiting, "{}", prompt);
        }
        let reading = apt.read(&screen(
            "Configuration file '/etc/ssh/sshd_config'\n\
             \x20==> Modified (by you or by a script) since installation.\n\
             \x20==> Package distributor has shipped an updated version.\n\
             \x20The default action is to keep your current version.\n\
             *** sshd_config (Y/I/N/O/D/Z) [default=N] ? ",
        ));
        assert_eq!(reading.state, TaskState::Waiting);
        // Once answered, it goes on.
        let reading = apt.read(&screen(
            "*** sshd_config (Y/I/N/O/D/Z) [default=N] ? N\n\
             Setting up openssh-server (1:9.6p1-3ubuntu13.5) ...",
        ));
        assert_eq!(reading.state, TaskState::Running);

        let brew = bundled("brew");
        let reading = brew.read(&screen(
            "==> Running installer for example with `sudo` (which may request your password)...\n\
             Password:",
        ));
        assert_eq!(reading.state, TaskState::Waiting);
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
    fn brew_downloading_in_parallel_says_how_far_the_whole_lot_is() {
        let brew = bundled("brew");
        // A line each, redrawn in place: 10.0 + 2.1 of 25.0 + 2.1 MB, and
        // one whose size is not known yet.
        let reading = brew.read(&screen(
            "==> Fetching downloads for: node, ripgrep and python@3.13\n\
             \u{2714}\u{FE0E} Bottle Manifest node (21.0.0)                 Downloaded   12.3KB/ 12.3KB\n\
             \u{280B} Bottle node (21.0.0)          ##########       Downloading  10.0MB/ 25.0MB\n\
             \u{2714}\u{FE0E} Bottle ripgrep (14.1.1)                       Downloaded    2.1MB/  2.1MB\n\
             \u{2819} Bottle python@3.13 (3.13.1)                    Downloading   1.2MB/-------",
        ));
        assert_eq!(reading.state, TaskState::Running);
        assert_eq!(reading.percent, Some(44));
        assert_eq!(reading.detail.as_deref(), Some("1 of 3 downloaded"));

        // A cask, and a pane too narrow for the version: counted all the same.
        let reading = brew.read(&screen(
            "==> Fetching downloads for: firefox\n\
             \u{280B} Cask firefox (130.0)     ########       Downloading  40.0MB/ 80.0MB\n\
             \u{280B} Bottle pyth ####  Downloading  20.0MB/ 20.0MB",
        ));
        assert_eq!(reading.percent, Some(60));
        assert_eq!(reading.detail.as_deref(), Some("1 of 2 downloaded"));

        // Lines an interrupted run left above this run's heading are not
        // this run's.
        let reading = brew.read(&screen(
            "==> Fetching downloads for: foo\n\
             \u{280B} Bottle foo (1.0)      ####         Downloading  10.0MB/ 25.0MB\n\
             ^C\n\
             $ brew install foo\n\
             ==> Fetching downloads for: foo\n\
             \u{2714}\u{FE0E} Bottle foo (1.0)                    Downloaded   25.0MB/ 25.0MB\n\
             ==> Pouring foo--1.0.arm64_sonoma.bottle.tar.gz",
        ));
        assert_eq!(reading.percent, None);
        assert_eq!(reading.detail.as_deref(), Some("Installing foo"));

        // All of it down, and on to the next step: that step is what it says.
        let reading = brew.read(&screen(
            "\u{2714}\u{FE0E} Bottle node (21.0.0)                          Downloaded   25.0MB/ 25.0MB\n\
             \u{2714}\u{FE0E} Bottle ripgrep (14.1.1)                       Downloaded    2.1MB/  2.1MB\n\
             ==> Pouring node--21.0.0.arm64_sonoma.bottle.tar.gz",
        ));
        assert_eq!(reading.percent, None);
        assert_eq!(reading.detail.as_deref(), Some("Installing node"));
    }

    #[test]
    fn a_summing_rule_adds_up_counts_too() {
        let rules = compile(
            r#"
            id = "demo"
            programs = ["demo"]
            [[rules]]
            id = "jobs"
            sum = true
            region = "bottom_non_empty_lines(5)"
            line_regex = ['^job \S+ (?P<done>\d+)/(?P<total>\d+)$']
            detail = "{lines_done} of {lines} jobs"
            "#,
            "test",
        )
        .unwrap();
        let reading = rules.read(&screen("job a 3/3\njob b 1/4\njob c 0/5"));
        assert_eq!(reading.count, Some((4, 12)));
        assert_eq!(reading.detail.as_deref(), Some("1 of 3 jobs"));
        assert_eq!(rules.read(&screen("job a 3/3\njob b 4/4")).detail, None);
    }

    #[test]
    fn sizes_read_in_their_units() {
        assert_eq!(parse_size(" 10.0MB"), Some(10e6));
        assert_eq!(parse_size("512.0 B"), Some(512.0));
        assert_eq!(parse_size("2KiB"), Some(2048.0));
        assert_eq!(parse_size("-------"), None);
        assert_eq!(parse_size("3 parsecs"), None);
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
