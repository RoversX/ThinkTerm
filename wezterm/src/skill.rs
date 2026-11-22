//! `thinkterm skill …`: put ThinkTerm's agent skill, and a hook that tells
//! an agent starting in a ThinkTerm pane where it is, into the coding agents
//! on this machine.

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The skill, as kept in the repository's `skills/thinkterm`.
const SKILL: &str = include_str!("../../skills/thinkterm/SKILL.md");
/// What the skill has beside it, which it sends an agent to read: making
/// a plugin.
const PLUGINS: &str = include_str!("../../skills/thinkterm/plugins.md");
/// The files beside SKILL.md, by name, written and removed with it.
const BESIDE: [(&str, &str); 1] = [("plugins.md", PLUGINS)];
/// How each of [`BESIDE`] begins, of whatever version: one that does not is
/// someone else's, and left alone.
fn is_beside_ours(text: &str) -> bool {
    text.starts_with("# Making a ThinkTerm plugin")
}

/// What the session-start hook runs: the context of the pane the agent
/// starts in, from the ThinkTerm that pane belongs to. Outside a ThinkTerm
/// pane, or when ThinkTerm does not answer, it prints nothing and succeeds,
/// so the agent starts as it would without it.
const HOOK_COMMAND: &str = "[ -n \"$WEZTERM_EXECUTABLE_DIR\" ] && \
     \"$WEZTERM_EXECUTABLE_DIR/thinkterm\" cli --no-auto-start agent context 2>/dev/null || true";

/// Seconds an agent waits for the hook at most.
const HOOK_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Parser, Clone)]
pub struct SkillCommand {
    #[command(subcommand)]
    sub: SkillSubCommand,
}

#[derive(Debug, Parser, Clone)]
enum SkillSubCommand {
    /// Install or update the skill and the session-start hook in the coding
    /// agents found on this machine. Safe to run again.
    #[command(name = "setup")]
    Setup(SetupArgs),

    /// Show what is installed, changing nothing
    #[command(name = "status")]
    Status(AgentArgs),

    /// Take ThinkTerm's skill and hook out of the agents
    #[command(name = "remove")]
    Remove(AgentArgs),

    /// Print the skill, for an agent `setup` does not know
    #[command(name = "show")]
    Show,
}

#[derive(Debug, Parser, Clone)]
struct AgentArgs {
    /// Only this agent, whether or not it was found
    #[arg(long, value_enum)]
    agent: Option<Agent>,
}

#[derive(Debug, Parser, Clone)]
struct SetupArgs {
    #[command(flatten)]
    agents: AgentArgs,

    /// Say what would change without changing anything
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Agent {
    Claude,
    Codex,
}

impl Agent {
    const ALL: [Agent; 2] = [Agent::Claude, Agent::Codex];

    fn name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
        }
    }

    /// The agent's own folder, wherever its environment moved it.
    fn home(self) -> PathBuf {
        let (var, default) = match self {
            Agent::Claude => ("CLAUDE_CONFIG_DIR", ".claude"),
            Agent::Codex => ("CODEX_HOME", ".codex"),
        };
        std::env::var_os(var)
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| config::HOME_DIR.join(default))
    }

    fn skill_file(self) -> PathBuf {
        let skills = match self {
            Agent::Claude => self.home().join("skills"),
            // Codex reads the folder agents share.
            Agent::Codex => config::HOME_DIR.join(".agents").join("skills"),
        };
        skills.join("thinkterm").join("SKILL.md")
    }

    fn hooks_file(self) -> PathBuf {
        match self {
            Agent::Claude => self.home().join("settings.json"),
            Agent::Codex => self.home().join("hooks.json"),
        }
    }
}

impl AgentArgs {
    /// The agents named, or else the ones that have a folder here.
    fn agents(&self) -> Vec<Agent> {
        match self.agent {
            Some(agent) => vec![agent],
            None => Agent::ALL
                .iter()
                .copied()
                .filter(|agent| agent.home().is_dir())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Current,
    Outdated,
    Missing,
    /// Something else goes by the name.
    Foreign,
    Unreadable,
}

impl State {
    fn label(self) -> &'static str {
        match self {
            State::Current => "current",
            State::Outdated => "outdated",
            State::Missing => "missing",
            State::Foreign => "someone else's",
            State::Unreadable => "unreadable",
        }
    }
}

impl SkillCommand {
    pub fn run(&self) -> Result<()> {
        match &self.sub {
            SkillSubCommand::Setup(args) => setup(&args.agents.agents(), args.dry_run),
            SkillSubCommand::Status(args) => status(&args.agents()),
            SkillSubCommand::Remove(args) => remove(&args.agents()),
            SkillSubCommand::Show => {
                let out = std::io::stdout();
                let mut out = out.lock();
                write!(out, "{SKILL}").map_err(crate::stdout_error)?;
                for (name, text) in BESIDE {
                    write!(out, "\n<!-- {name}, beside SKILL.md -->\n\n{text}")
                        .map_err(crate::stdout_error)?;
                }
                Ok(())
            }
        }
    }
}

fn no_agents(agents: &[Agent]) -> bool {
    if agents.is_empty() {
        println!("No Claude Code or Codex found here; name one with --agent.");
    }
    agents.is_empty()
}

fn setup(agents: &[Agent], dry_run: bool) -> Result<()> {
    if no_agents(agents) {
        return Ok(());
    }
    for &agent in agents {
        println!("{}", agent.name());
        let skill = agent.skill_file();
        let line = match skill_state(&skill) {
            State::Current => "up to date",
            State::Foreign => "left alone: another skill is called thinkterm",
            State::Unreadable => "left alone: could not be read",
            State::Missing | State::Outdated if dry_run => "would be written",
            State::Missing | State::Outdated => {
                write_skill(&skill)?;
                "written"
            }
        };
        println!("  skill  {line}  {}", shown(&skill));

        let hooks = agent.hooks_file();
        if cfg!(windows) {
            println!("  hook   not on Windows yet");
            continue;
        }
        let line = match read_settings(&hooks) {
            Err(err) => format!("left alone: {err:#}"),
            Ok(settings) => {
                let updated = with_hook(settings.clone());
                if updated == settings {
                    "up to date".to_string()
                } else if dry_run {
                    "would be added".to_string()
                } else {
                    write_file(&hooks, &updated_text(&hooks, &updated)?)?;
                    "added".to_string()
                }
            }
        };
        println!("  hook   {line}  {}", shown(&hooks));
    }
    if !dry_run {
        println!();
        if agents.contains(&Agent::Claude) {
            println!("Claude Code runs hooks only in folders you have trusted.");
        }
        if agents.contains(&Agent::Codex) {
            println!("Codex runs a new hook once you approve it with /hooks.");
        }
    }
    Ok(())
}

fn status(agents: &[Agent]) -> Result<()> {
    if no_agents(agents) {
        return Ok(());
    }
    for &agent in agents {
        println!("{}", agent.name());
        let skill = agent.skill_file();
        println!("  skill  {}  {}", skill_state(&skill).label(), shown(&skill));
        let hooks = agent.hooks_file();
        let state = if cfg!(windows) {
            "not on Windows yet"
        } else {
            hook_state(&hooks).label()
        };
        println!("  hook   {state}  {}", shown(&hooks));
    }
    Ok(())
}

fn remove(agents: &[Agent]) -> Result<()> {
    if no_agents(agents) {
        return Ok(());
    }
    for &agent in agents {
        println!("{}", agent.name());
        let skill = agent.skill_file();
        let line = match skill_state(&skill) {
            State::Current | State::Outdated => {
                std::fs::remove_file(&skill)
                    .with_context(|| format!("remove {}", skill.display()))?;
                if let Some(dir) = skill.parent() {
                    for (name, _) in BESIDE {
                        let path = dir.join(name);
                        // Only one of ours: something else by the name stays.
                        let ours =
                            std::fs::read_to_string(&path).is_ok_and(|text| is_beside_ours(&text));
                        if !ours {
                            continue;
                        }
                        std::fs::remove_file(&path).with_context(|| format!("remove {name}"))?;
                    }
                    // Only when nothing else was put beside it.
                    let _ = std::fs::remove_dir(dir);
                }
                "removed"
            }
            State::Missing => "not there",
            State::Foreign => "left alone: another skill is called thinkterm",
            State::Unreadable => "left alone: could not be read",
        };
        println!("  skill  {line}  {}", shown(&skill));

        let hooks = agent.hooks_file();
        let line = match read_settings(&hooks) {
            Err(err) => format!("left alone: {err:#}"),
            Ok(settings) => {
                let updated = without_hook(settings.clone());
                if updated == settings {
                    "not there".to_string()
                } else {
                    write_file(&hooks, &updated_text(&hooks, &updated)?)?;
                    "removed".to_string()
                }
            }
        };
        println!("  hook   {line}  {}", shown(&hooks));
    }
    Ok(())
}

/// How the skill at `path`, its SKILL.md, is: current only when what is
/// beside it is too, and someone else's when what is beside it under one of
/// its names is.
fn skill_state(path: &Path) -> State {
    let dir = path.parent().unwrap_or(path);
    let foreign_beside = BESIDE.iter().any(|(name, _)| {
        std::fs::read_to_string(dir.join(name)).is_ok_and(|text| !is_beside_ours(&text))
    });
    match std::fs::read_to_string(path) {
        Ok(_) if foreign_beside => State::Foreign,
        Ok(text) if text == SKILL => {
            let dir = path.parent().unwrap_or(path);
            let beside = BESIDE.iter().all(|(name, text)| {
                std::fs::read_to_string(dir.join(name)).is_ok_and(|have| have == *text)
            });
            if beside {
                State::Current
            } else {
                State::Outdated
            }
        }
        Ok(text) if is_thinkterm_skill(&text) => State::Outdated,
        Ok(_) => State::Foreign,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => State::Missing,
        Err(_) => State::Unreadable,
    }
}

/// Writes the skill at `path`, its SKILL.md, and what goes beside it --
/// SKILL.md last, so a skill that reads as current has all of it.
fn write_skill(path: &Path) -> Result<()> {
    let dir = path.parent().context("no folder")?;
    for (name, text) in BESIDE {
        write_file(&dir.join(name), text)?;
    }
    write_file(path, SKILL)
}

/// Whether `text` is a ThinkTerm skill of some version: one named that in
/// its front matter.
fn is_thinkterm_skill(text: &str) -> bool {
    text.lines()
        .take_while(|line| !line.is_empty())
        .any(|line| line.trim() == "name: thinkterm")
}

fn hook_state(path: &Path) -> State {
    match read_settings(path) {
        Err(_) => State::Unreadable,
        Ok(settings) => {
            let commands = hook_commands(&settings);
            if commands.is_empty() {
                State::Missing
            } else if commands.iter().all(|command| *command == HOOK_COMMAND) {
                State::Current
            } else {
                State::Outdated
            }
        }
    }
}

/// The agent's settings, or an empty object when it has none yet.
fn read_settings(path: &Path) -> Result<Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    let settings: Value =
        serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))?;
    if !settings.is_object() {
        bail!("{} is not a JSON object", path.display());
    }
    Ok(settings)
}

/// Whether `command` is a ThinkTerm session-start hook, of any version.
fn is_thinkterm_hook(command: &str) -> bool {
    command.contains("thinkterm") && command.contains("agent context")
}

/// The commands of ThinkTerm's session-start hooks in `settings`.
fn hook_commands(settings: &Value) -> Vec<&str> {
    settings["hooks"]["SessionStart"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .filter_map(|hook| hook["command"].as_str())
        .filter(|command| is_thinkterm_hook(command))
        .collect()
}

/// `settings` with ThinkTerm's session-start hook in it once, as this
/// version writes it, and everything else as it was.
fn with_hook(settings: Value) -> Value {
    if hook_commands(&settings) == [HOOK_COMMAND] {
        return settings;
    }
    let mut settings = without_hook(settings);
    let hooks = settings
        .as_object_mut()
        .expect("settings are an object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let session_start = hooks
        .as_object_mut()
        .expect("just made an object")
        .entry("SessionStart")
        .or_insert_with(|| json!([]));
    if !session_start.is_array() {
        *session_start = json!([]);
    }
    session_start
        .as_array_mut()
        .expect("just made an array")
        .push(json!({
            "hooks": [{
                "type": "command",
                "command": HOOK_COMMAND,
                "timeout": HOOK_TIMEOUT_SECS,
            }]
        }));
    settings
}

/// `settings` without ThinkTerm's session-start hooks, dropping what they
/// leave empty and nothing else.
fn without_hook(mut settings: Value) -> Value {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return settings;
    };
    let Some(groups) = hooks.get_mut("SessionStart").and_then(Value::as_array_mut) else {
        return settings;
    };
    let mut changed = false;
    groups.retain_mut(|group| {
        let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            return true;
        };
        let len = list.len();
        list.retain(|hook| !hook["command"].as_str().is_some_and(is_thinkterm_hook));
        if list.len() == len {
            return true;
        }
        changed = true;
        !list.is_empty()
    });
    if !changed {
        return settings;
    }
    if groups.is_empty() {
        hooks.remove("SessionStart");
    }
    if hooks.is_empty() {
        settings.as_object_mut().expect("has hooks").remove("hooks");
    }
    settings
}

fn to_text(settings: &Value) -> Result<String> {
    let mut text = serde_json::to_string_pretty(settings)?;
    text.push('\n');
    Ok(text)
}

/// The settings file at `path` with `updated`'s `hooks` written in, and
/// every other byte as it was: rewriting the whole object would sort a
/// person's keys.
fn updated_text(path: &Path, updated: &Value) -> Result<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => return to_text(updated),
    };
    with_top_level(&text, "hooks", updated.get("hooks"))
        .with_context(|| format!("edit {}", path.display()))
}

/// A top-level member of a JSON object's text: where its key starts and
/// where its value starts and ends.
struct Member {
    key: String,
    start: usize,
    value: std::ops::Range<usize>,
}

/// The members of the JSON object `text`, in order, with the offset of its
/// closing brace.
fn top_level_members(text: &str) -> Result<(Vec<Member>, usize)> {
    let bytes = text.as_bytes();
    let skip_ws = |mut at: usize| {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        at
    };
    // The end of the string starting at `at`, past its closing quote.
    let string_end = |mut at: usize| -> Result<usize> {
        at += 1;
        while at < bytes.len() {
            match bytes[at] {
                b'\\' => at += 2,
                b'"' => return Ok(at + 1),
                _ => at += 1,
            }
        }
        bail!("unterminated string")
    };
    let mut at = skip_ws(0);
    if bytes.get(at) != Some(&b'{') {
        bail!("not a JSON object");
    }
    at = skip_ws(at + 1);
    let mut members = vec![];
    loop {
        match bytes.get(at) {
            Some(b'}') => return Ok((members, at)),
            Some(b'"') => {}
            _ => bail!("unexpected text at {at}"),
        }
        let start = at;
        let key_end = string_end(at)?;
        let key: String = serde_json::from_str(&text[at..key_end])?;
        at = skip_ws(key_end);
        if bytes.get(at) != Some(&b':') {
            bail!("expected ':' at {at}");
        }
        let value_start = skip_ws(at + 1);
        let mut end = value_start;
        let mut depth = 0usize;
        while end < bytes.len() {
            match bytes[end] {
                b'"' => {
                    end = string_end(end)?;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                b'{' | b'[' => depth += 1,
                b'}' | b']' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        end += 1;
                        break;
                    }
                }
                b',' | b'}' if depth == 0 => break,
                _ => {}
            }
            end += 1;
        }
        let value_end = value_start + text[value_start..end].trim_end().len();
        members.push(Member {
            key,
            start,
            value: value_start..value_end,
        });
        at = skip_ws(end);
        match bytes.get(at) {
            Some(b',') => at = skip_ws(at + 1),
            Some(b'}') => {}
            _ => bail!("expected ',' or '}}' at {at}"),
        }
    }
}

/// `text`, a JSON object, with its member `key` set to `value`, added at
/// the end, or taken out when `value` is None.
fn with_top_level(text: &str, key: &str, value: Option<&Value>) -> Result<String> {
    let (members, close) = top_level_members(text)?;
    let index = members.iter().position(|member| member.key == key);
    let Some(value) = value else {
        let Some(index) = index else {
            return Ok(text.to_string());
        };
        // Out with the comma that joined it to its neighbour.
        let range = if index > 0 {
            members[index - 1].value.end..members[index].value.end
        } else if let Some(next) = members.get(1) {
            members[0].start..next.start
        } else {
            let open = text.find('{').expect("an object") + 1;
            open..close
        };
        let mut edited = text.to_string();
        edited.replace_range(range, "");
        return Ok(edited);
    };
    // One level in, like the members around it.
    let value_text = serde_json::to_string_pretty(value)?.replace('\n', "\n  ");
    let mut edited = text.to_string();
    match index {
        Some(index) => edited.replace_range(members[index].value.clone(), &value_text),
        None => {
            let member = format!("\"{key}\": {value_text}");
            match members.last() {
                Some(last) => edited.insert_str(last.value.end, &format!(",\n  {member}")),
                None => edited.insert_str(close, &format!("\n  {member}\n")),
            }
        }
    }
    Ok(edited)
}

/// Replace `path` with `contents` in one step, keeping its permissions.
fn write_file(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().context("no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut file = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("write in {}", dir.display()))?;
    file.write_all(contents.as_bytes())?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(file.path(), meta.permissions());
    }
    file.persist(path)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// `path` with the home folder written as `~`.
fn shown(path: &Path) -> String {
    match path.strip_prefix(&*config::HOME_DIR) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hook_goes_in_once_beside_everything_else() {
        let settings = json!({
            "model": "opus",
            "hooks": {
                "SessionStart": [{"matcher": "startup", "hooks": [{"type": "command", "command": "echo hi"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "echo bye"}]}]
            }
        });
        let once = with_hook(settings.clone());
        assert_eq!(hook_commands(&once), [HOOK_COMMAND]);
        assert_eq!(with_hook(once.clone()), once);
        assert_eq!(once["model"], "opus");
        assert_eq!(once["hooks"]["Stop"], settings["hooks"]["Stop"]);
        assert_eq!(once["hooks"]["SessionStart"][0], settings["hooks"]["SessionStart"][0]);
        assert_eq!(without_hook(once), settings);
    }

    #[test]
    fn an_older_hook_is_replaced_not_added_to() {
        let settings = json!({"hooks": {"SessionStart": [{"hooks": [
            {"type": "command", "command": "thinkterm cli agent context"}
        ]}]}});
        let updated = with_hook(settings);
        assert_eq!(hook_commands(&updated), [HOOK_COMMAND]);
        assert_eq!(updated["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn removing_the_hook_leaves_no_empty_shells() {
        let settings = with_hook(json!({"theme": "dark"}));
        assert_eq!(without_hook(settings), json!({"theme": "dark"}));
        assert_eq!(without_hook(json!({})), json!({}));
    }

    #[test]
    fn only_the_hooks_member_of_a_settings_file_changes() {
        let text = "{\n  \"theme\": \"dark\",\n  \"env\": {\"A\": \"1\"},\n  \"model\": \"opus\"\n}\n";
        let settings: Value = serde_json::from_str(text).unwrap();
        let added = with_top_level(text, "hooks", with_hook(settings).get("hooks")).unwrap();
        assert!(added.starts_with(
            "{\n  \"theme\": \"dark\",\n  \"env\": {\"A\": \"1\"},\n  \"model\": \"opus\",\n  \"hooks\": {"
        ));
        let parsed: Value = serde_json::from_str(&added).unwrap();
        assert_eq!(hook_commands(&parsed), [HOOK_COMMAND]);
        // Out again, the file is what it was.
        assert_eq!(with_top_level(&added, "hooks", None).unwrap(), text);

        let first = "{\n  \"hooks\": {},\n  \"theme\": \"dark\"\n}\n";
        let replaced = with_top_level(first, "hooks", Some(&json!({"Stop": []}))).unwrap();
        assert!(replaced.starts_with("{\n  \"hooks\": {\n    \"Stop\": []\n  },\n  \"theme\""));
        assert_eq!(
            with_top_level(first, "hooks", None).unwrap(),
            "{\n  \"theme\": \"dark\"\n}\n"
        );
        assert_eq!(
            with_top_level("{}\n", "hooks", Some(&json!({}))).unwrap(),
            "{\n  \"hooks\": {}\n}\n"
        );
    }

    #[test]
    fn the_skill_states() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thinkterm").join("SKILL.md");
        assert_eq!(skill_state(&path), State::Missing);
        write_file(&path, SKILL).unwrap();
        assert_eq!(skill_state(&path), State::Outdated, "nothing beside it yet");
        write_skill(&path).unwrap();
        assert_eq!(skill_state(&path), State::Current);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("thinkterm").join("plugins.md")).unwrap(),
            PLUGINS
        );
        write_file(
            &dir.path().join("thinkterm").join("plugins.md"),
            "# Making a ThinkTerm plugin\n\nolder\n",
        )
        .unwrap();
        assert_eq!(
            skill_state(&path),
            State::Outdated,
            "what is beside it is old"
        );
        write_skill(&path).unwrap();
        assert_eq!(skill_state(&path), State::Current);
        // Someone else's file by the name: the skill is theirs to keep.
        write_file(&dir.path().join("thinkterm").join("plugins.md"), "my notes").unwrap();
        assert_eq!(skill_state(&path), State::Foreign);
        write_skill(&path).unwrap();
        assert!(is_beside_ours(PLUGINS));
        write_file(&path, "---\nname: thinkterm\ndescription: old\n---\nolder text\n").unwrap();
        assert_eq!(skill_state(&path), State::Outdated);
        write_file(&path, "---\nname: something-else\n---\n").unwrap();
        assert_eq!(skill_state(&path), State::Foreign);
        assert!(is_thinkterm_skill(SKILL));
        assert!(SKILL.contains("plugins.md"), "it sends an agent to read it");
        // Read whole by an agent, as its skill tool hands it over.
        assert!(PLUGINS.len() <= 8 * 1024, "{} bytes", PLUGINS.len());
    }

    #[test]
    fn the_hook_states() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(hook_state(&path), State::Missing);
        write_file(&path, &to_text(&with_hook(json!({}))).unwrap()).unwrap();
        assert_eq!(hook_state(&path), State::Current);
        write_file(&path, "not json").unwrap();
        assert_eq!(hook_state(&path), State::Unreadable);
    }
}
