//! Process-fact → agent matching rules, ported from herdr's
//! identification layer (research/herdr/src/detect/mod.rs).
//!
//! Layering contract: this module performs no pane or process-table IO.
//! The executable path and an argv thunk come in as arguments, the
//! manifest alias table comes in as a lookup closure, and the only
//! filesystem access is the final symlink-resolution tier. The usual
//! maintenance edits — a new runtime, a new flag, a new package layout,
//! a new process alias — are all table changes at the top of this file.

/// Executables that are interpreters or shells: their own name says
/// nothing about the agent, so only their argv can. Python is matched
/// separately because of its versioned basenames (`python3.12`).
const GENERIC_RUNTIMES: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "fish",
    "tmux",
    "node",
    "bun",
    "cmd",
    "powershell",
    "pwsh",
];

/// Stripped from a candidate name before the alias lookup, so `pi.js`
/// and `codex.cmd` read as `pi` and `codex`. First match wins.
const STRIP_SUFFIXES: &[&str] = &[".exe", ".cmd", ".bat", ".ps1", ".js"];

/// Interpreter options that consume the next argv slot, so the argv scan
/// does not mistake an option value for the script path
/// (`node -r ./preload.js real-script.js`).
const OPTIONS_TAKING_VALUE: &[&str] = &[
    "-r",
    "--require",
    "--loader",
    "--import",
    "--experimental-loader",
    "--inspect-port",
    "-W",
    "-X",
    "-S",
    "-L",
    "-o",
];

/// Script paths whose basename is generic (`cli.js`, `index.js`) but
/// whose package directory names the agent. Matched as a normalized
/// path-component window; the last component is compared after suffix
/// stripping. The id is re-checked against the manifest table so an
/// entry without a manifest can never claim a pane.
const KNOWN_PACKAGE_PATHS: &[(&[&str], &str)] = &[
    (
        &["node_modules", "@earendil-works", "pi-coding-agent", "dist", "cli"],
        "pi",
    ),
    (
        &[
            "node_modules",
            "@earendil-works",
            "pi-coding-agent",
            "dist",
            "bundle",
            "cli",
        ],
        "pi",
    ),
];

/// Process names herdr recognizes in its code-side table that our
/// manifest alias lists do not carry (the manifests assume herdr's own
/// table exists). The id is re-checked against the manifests.
const EXTRA_PROCESS_ALIASES: &[(&str, &str)] = &[
    ("copilot", "github-copilot"),
    ("opencode2", "opencode"),
];

/// Identify the agent behind a foreground process-group leader.
///
/// `leader_path` is the leader's executable path (already normalized of
/// Linux's " (deleted)" decoration). `argv` is fetched only when the
/// executable turns out to be a generic interpreter. `lookup` maps a
/// normalized process name to a manifest id (id or alias).
pub(crate) fn identify(
    leader_path: &str,
    argv: &mut dyn FnMut() -> Option<Vec<String>>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let name = normalized_lookup_name(basename(leader_path));
    if is_generic_runtime(&name) {
        // A runtime name can never identify by itself; unwrap or give up,
        // so no manifest alias can ever claim every `node` on the box.
        let argv = argv()?;
        // Node's `process.title = "pi"` rewrites the argv block in place,
        // and the OS reads the title back — often as the only surviving
        // token, the original script path clobbered. That self-declared
        // title is the strongest signal there is, so consult argv[0]
        // before scanning for a script path. An unretitled interpreter
        // has argv[0] = "node"/"bash"/…, which no manifest aliases.
        if let Some(first) = argv.first() {
            if let Some(id) = resolve_name(&normalized_lookup_name(basename(first)), lookup) {
                return Some(id);
            }
        }
        return wrapped_agent(&name, &argv, lookup);
    }
    if let Some(id) = resolve_name(&name, lookup) {
        return Some(id);
    }
    versioned_launcher_agent(leader_path, &name, lookup)
}

/// One alias resolution: the manifest table first, then the code-side
/// supplement (whose target must itself exist in the manifest table).
fn resolve_name(name: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(id) = lookup(name) {
        return Some(id);
    }
    EXTRA_PROCESS_ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .and_then(|(_, id)| lookup(id))
}

/// The interpreter's argv decides. Eval payloads (`node -e …`,
/// `python -c …`, `sh -c …`) are never mined for agent names — a token
/// merely *looking* like an agent must not classify the pane.
fn wrapped_agent(
    runtime: &str,
    argv: &[String],
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    match runtime {
        "node" => cursor_bundled_node_agent(argv, lookup)
            .or_else(|| script_arg_agent(argv, &["-e", "--eval", "-p", "--print"], &[], lookup)),
        "bun" => script_arg_agent(argv, &["-e", "--eval", "-p", "--print"], &[], lookup),
        name if is_python_runtime(name) => script_arg_agent(argv, &["-c"], &["-m"], lookup),
        "sh" | "bash" | "zsh" | "fish" => script_arg_agent(argv, &["-c"], &[], lookup),
        "cmd" => cmd_arg_agent(argv, lookup),
        "powershell" | "pwsh" => powershell_arg_agent(argv, lookup),
        // tmux wraps a server, not an agent.
        _ => None,
    }
}

/// npm on Windows installs `.cmd` batch shims run as
/// `cmd /c C:\…\codex.cmd --model x`: the agent's name lives in the
/// command *text* after `/c`, not in argv proper.
fn cmd_arg_agent(argv: &[String], lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        let flag = arg.trim_matches('"').to_lowercase();
        match flag.as_str() {
            "/c" | "/k" => {
                return args
                    .next()
                    .and_then(|command| command_text_agent(command, lookup));
            }
            "/d" | "/s" | "/q" | "/a" | "/u" | "/e:on" | "/e:off" | "/f:on" | "/f:off"
            | "/v:on" | "/v:off" => continue,
            _ => {}
        }
    }
    None
}

fn powershell_arg_agent(
    argv: &[String],
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        let flag = arg.trim_matches('"').to_lowercase();
        match flag.as_str() {
            "-file" | "-f" | "/file" => {
                return args.next().and_then(|path| agent_from_path_token(path, lookup));
            }
            "-command" | "-c" | "/command" | "/c" => {
                return args
                    .next()
                    .and_then(|command| command_text_agent(command, lookup));
            }
            // Base64 payloads are opaque on purpose — never guess.
            "-encodedcommand" | "-enc" | "/encodedcommand" | "/enc" => return None,
            "-configurationname" | "-executionpolicy" | "-outputformat" | "-psconsolefile"
            | "-version" | "-windowstyle" | "-workingdirectory" => {
                let _ = args.next();
            }
            _ if flag.starts_with('-') || flag.starts_with('/') => {}
            _ => return agent_from_path_token(arg, lookup),
        }
    }
    None
}

/// First real token of a shell command string, skipping the call-forms
/// (`&`, `.`, `call`) that prefix the actual program.
fn command_text_agent(command: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut rest = command;
    while let Some((token, next)) = command_text_token(rest) {
        let token = token.trim();
        if token.eq_ignore_ascii_case("&")
            || token.eq_ignore_ascii_case(".")
            || token.eq_ignore_ascii_case("call")
        {
            rest = next;
            continue;
        }
        return agent_from_path_token(token, lookup);
    }
    None
}

/// Split one quote-aware token off the front of a command string.
fn command_text_token(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    let first = input.chars().next()?;
    if first == '"' || first == '\'' {
        let start = first.len_utf8();
        if let Some(end) = input[start..].find(first) {
            let end = start + end;
            return Some((&input[start..end], &input[end + first.len_utf8()..]));
        }
        return Some((&input[start..], ""));
    }

    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    Some((&input[..end], &input[end..]))
}

/// Find the script path in an interpreter's argv: skip options (consuming
/// values where the option takes one), bail out entirely on eval/module
/// flags, honor `--` as "next token is the script".
fn script_arg_agent(
    argv: &[String],
    eval_flags: &[&str],
    module_flags: &[&str],
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            return args.next().and_then(|token| agent_from_path_token(token, lookup));
        }
        if flag_matches(arg, eval_flags) || flag_matches(arg, module_flags) {
            return None;
        }
        if arg.starts_with('-') {
            if OPTIONS_TAKING_VALUE.contains(&arg.as_str()) {
                let _ = args.next();
            }
            continue;
        }
        return agent_from_path_token(arg, lookup);
    }
    None
}

/// `-e`, `-epayload`, `--eval`, and `--eval=payload` all count.
fn flag_matches(arg: &str, flags: &[&str]) -> bool {
    flags.iter().any(|flag| {
        arg == *flag
            || (!flag.starts_with("--") && arg.len() > flag.len() && arg.starts_with(flag))
            || (flag.starts_with("--")
                && arg.starts_with(flag)
                && arg.as_bytes().get(flag.len()) == Some(&b'='))
    })
}

/// Cursor ships its own node: `<pkg>/versions/<ver>/node[.exe]` running
/// `<same directory>/index.js`. Neither basename names the agent, so the
/// install layout has to — and only that exact layout: a system node
/// running some unrelated `…/cursor-agent/versions/x/index.js` lives in a
/// different parent directory and must not match.
fn cursor_bundled_node_agent(
    argv: &[String],
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let (runtime_parent, runtime_name) = path_parent_and_basename(argv.first()?)?;
    let (script_parent, script_name) = path_parent_and_basename(argv.get(1)?)?;
    if normalized_lookup_name(runtime_name) != "node"
        || !script_name.eq_ignore_ascii_case("index.js")
        || !runtime_parent.eq_ignore_ascii_case(script_parent)
    {
        return None;
    }
    let mut tail = runtime_parent
        .rsplit(['/', '\\'])
        .filter(|component| !component.is_empty());
    let (Some(version), Some(versions), Some(package)) = (tail.next(), tail.next(), tail.next())
    else {
        return None;
    };
    (package.eq_ignore_ascii_case("cursor-agent")
        && versions.eq_ignore_ascii_case("versions")
        && !version.trim().is_empty())
    .then(|| lookup("cursor-agent"))?
}

fn path_parent_and_basename(path: &str) -> Option<(&str, &str)> {
    let index = path.rfind(['/', '\\'])?;
    let parent = &path[..index];
    let base = &path[index + 1..];
    if parent.is_empty() || base.is_empty() {
        return None;
    }
    Some((parent, base))
}

/// Map a script-path token to an agent, in three tiers: exact basename
/// (after suffix stripping), known package-directory layout, and finally
/// symlink resolution for shims whose link name is generic but whose
/// target is not.
fn agent_from_path_token(token: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let trimmed = token.trim_matches(|c| matches!(c, '"' | '\''));
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return None;
    }
    resolve_name(&normalized_lookup_name(basename(trimmed)), lookup)
        .or_else(|| known_package_agent(trimmed, lookup))
        .or_else(|| symlink_target_agent(trimmed, lookup))
}

fn known_package_agent(path: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let components: Vec<String> = path
        .split(['/', '\\'])
        .filter(|component| !component.is_empty())
        .map(normalized_lookup_name)
        .collect();
    for (window, id) in KNOWN_PACKAGE_PATHS {
        // `windows(0)` panics; an empty table entry is a table-editing
        // mistake, not a reason to take the process down.
        if window.is_empty() {
            continue;
        }
        if components
            .windows(window.len())
            .any(|candidate| candidate == *window)
        {
            return lookup(id);
        }
    }
    None
}

/// The one filesystem touch in this module. Bare names never reach the
/// filesystem — only multi-component paths can be shims worth resolving.
fn symlink_target_agent(token: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let path = std::path::Path::new(token);
    if path.components().count() < 2 {
        return None;
    }
    let resolved = std::fs::canonicalize(path).ok()?;
    let base = resolved.file_name()?.to_str()?;
    resolve_name(&normalized_lookup_name(base), lookup)
}

/// Version-managed launchers exec a binary named after the version —
/// e.g. Claude Code runs as `~/.local/share/claude/versions/2.1.239` —
/// so the agent's name only appears as a parent directory. Directory
/// components only vouch for that layout: without the version-like
/// basename gate, any executable under an alias-named directory would be
/// misidentified (everything in `/home/pi/.local/bin`, say).
fn versioned_launcher_agent(
    path: &str,
    name: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    if !looks_like_version(name) {
        return None;
    }
    path.rsplit(['/', '\\'])
        .skip(1)
        .take(2)
        .find_map(|component| resolve_name(&component.to_lowercase(), lookup))
}

/// A launcher-style version basename: starts with a digit, rest is
/// digits/letters/dots/dashes ("2.1.239", "1.0.0-rc1").
fn looks_like_version(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

fn is_generic_runtime(name: &str) -> bool {
    is_python_runtime(name) || GENERIC_RUNTIMES.contains(&name)
}

fn is_python_runtime(name: &str) -> bool {
    name == "python"
        || name.strip_prefix("python").is_some_and(|version| {
            !version.is_empty()
                && version
                    .split('.')
                    .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
        })
}

fn normalized_lookup_name(name: &str) -> String {
    let mut name = name.trim().to_lowercase();
    for suffix in STRIP_SUFFIXES {
        if name.ends_with(suffix) {
            name.truncate(name.len() - suffix.len());
            break;
        }
    }
    name
}

pub(crate) fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for the manifest id/alias table, so these tests are
    /// independent of which manifests are bundled.
    fn lookup(name: &str) -> Option<String> {
        match name {
            "claude" | "claude-code" => Some("claude".to_string()),
            "codex" => Some("codex".to_string()),
            "github-copilot" | "ghcs" => Some("github-copilot".to_string()),
            "cursor" | "cursor-agent" => Some("cursor".to_string()),
            "kimi" => Some("kimi".to_string()),
            "opencode" => Some("opencode".to_string()),
            "pi" => Some("pi".to_string()),
            _ => None,
        }
    }

    fn identify_with_argv(path: &str, argv: &[&str]) -> Option<String> {
        let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        let mut fetch = || Some(argv.clone());
        identify(path, &mut fetch, &lookup)
    }

    fn identify_no_argv(path: &str) -> Option<String> {
        let mut fetch = || None;
        identify(path, &mut fetch, &lookup)
    }

    #[test]
    fn plain_agent_binaries_identify_directly() {
        assert_eq!(identify_no_argv("/usr/local/bin/claude").as_deref(), Some("claude"));
        assert_eq!(identify_no_argv("C:\\tools\\codex.exe").as_deref(), Some("codex"));
        assert_eq!(identify_no_argv("/bin/zsh"), None);
        assert_eq!(identify_no_argv("/tmp/my-codex-helper"), None);
    }

    #[test]
    fn npm_shims_identify_through_the_interpreter_argv() {
        // `pi` installed via npm: the leader is node, the shim path rides
        // in argv[1]. This is the case the old basename-only identifier
        // was blind to.
        assert_eq!(
            identify_with_argv(
                "/opt/homebrew/bin/node",
                &["node", "/opt/homebrew/bin/pi"]
            )
            .as_deref(),
            Some("pi")
        );
        // Runtime options before the script are skipped; value-taking
        // options consume their value.
        assert_eq!(
            identify_with_argv(
                "/usr/bin/node",
                &[
                    "node",
                    "--max-old-space-size=4096",
                    "-r",
                    "/x/preload.js",
                    "/usr/local/bin/codex"
                ]
            )
            .as_deref(),
            Some("codex")
        );
        // `--` means "next token is the script".
        assert_eq!(
            identify_with_argv("/usr/bin/node", &["node", "--", "/x/pi"]).as_deref(),
            Some("pi")
        );
        // `.js` is stripped before the alias lookup.
        assert_eq!(
            identify_with_argv("/usr/bin/node", &["node", "/y/dist/pi.js"]).as_deref(),
            Some("pi")
        );
        // A bare runtime with no script stays unidentified.
        assert_eq!(identify_with_argv("/usr/bin/node", &["node"]), None);
        assert_eq!(identify_no_argv("/usr/bin/node"), None);
    }

    #[test]
    fn cmd_and_powershell_command_lines_unwrap() {
        // The npm .cmd shim shape.
        assert_eq!(
            identify_with_argv(
                "C:\\Windows\\System32\\cmd.exe",
                &["cmd", "/c", "C:\\Users\\x\\AppData\\Roaming\\npm\\codex.cmd --model gpt-5"]
            )
            .as_deref(),
            Some("codex")
        );
        // Quoted path with spaces, and a leading `call`.
        assert_eq!(
            identify_with_argv(
                "C:\\Windows\\System32\\cmd.exe",
                &["cmd", "/d", "/c", "call \"C:\\Program Files\\agents\\claude.cmd\" --resume"]
            )
            .as_deref(),
            Some("claude")
        );
        assert_eq!(
            identify_with_argv(
                "C:\\Program Files\\PowerShell\\7\\pwsh.exe",
                &["pwsh", "-File", "C:\\tools\\claude.ps1"]
            )
            .as_deref(),
            Some("claude")
        );
        assert_eq!(
            identify_with_argv(
                "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
                &["powershell", "-Command", "& 'C:\\agents\\pi.cmd' --serve"]
            )
            .as_deref(),
            Some("pi")
        );
        // Encoded payloads are opaque; option values must not be mistaken
        // for programs.
        assert_eq!(
            identify_with_argv(
                "C:\\Program Files\\PowerShell\\7\\pwsh.exe",
                &["pwsh", "-EncodedCommand", "YwBsAGEAdQBkAGUA"]
            ),
            None
        );
        assert_eq!(
            identify_with_argv(
                "C:\\Program Files\\PowerShell\\7\\pwsh.exe",
                &["pwsh", "-ExecutionPolicy", "Bypass", "-File", "C:\\x\\kimi.ps1"]
            )
            .as_deref(),
            Some("kimi")
        );
        // A bare cmd with no /c never identifies.
        assert_eq!(
            identify_with_argv("C:\\Windows\\System32\\cmd.exe", &["cmd"]),
            None
        );
    }

    #[test]
    fn cursor_bundled_node_identifies_by_install_layout() {
        // Unix layout.
        assert_eq!(
            identify_with_argv(
                "/home/x/.local/share/cursor-agent/versions/2.0.1/node",
                &[
                    "/home/x/.local/share/cursor-agent/versions/2.0.1/node",
                    "/home/x/.local/share/cursor-agent/versions/2.0.1/index.js"
                ]
            )
            .as_deref(),
            Some("cursor")
        );
        // Windows layout, backslashes and node.exe.
        assert_eq!(
            identify_with_argv(
                "C:\\Users\\x\\cursor-agent\\versions\\2.0.1\\node.exe",
                &[
                    "C:\\Users\\x\\cursor-agent\\versions\\2.0.1\\node.exe",
                    "C:\\Users\\x\\cursor-agent\\versions\\2.0.1\\index.js"
                ]
            )
            .as_deref(),
            Some("cursor")
        );
        // A system node running a lookalike path must not match: the
        // runtime does not live in the cursor-agent directory.
        assert_eq!(
            identify_with_argv(
                "/usr/bin/node",
                &["/usr/bin/node", "/tmp/cursor-agent/versions/x/index.js"]
            ),
            None
        );
    }

    /// Node's `process.title = "pi"` clobbers the argv block: the OS then
    /// reports argv as just `["pi"]`, the script path gone. The rewritten
    /// argv[0] is the agent's own self-declaration — trust it.
    #[test]
    fn retitled_interpreters_identify_by_their_new_title() {
        assert_eq!(
            identify_with_argv("/opt/homebrew/Cellar/node/26.7.0/bin/node", &["pi"]).as_deref(),
            Some("pi")
        );
        // An unretitled interpreter's argv[0] is the runtime name and
        // must not match anything.
        assert_eq!(
            identify_with_argv("/usr/bin/node", &["node", "/opt/tools/build.js"]),
            None
        );
    }

    #[test]
    fn eval_payloads_are_never_mined_for_agent_names() {
        assert_eq!(
            identify_with_argv("/usr/bin/node", &["node", "-e", "codex"]),
            None
        );
        assert_eq!(
            identify_with_argv("/usr/bin/node", &["node", "--eval=codex"]),
            None
        );
        assert_eq!(
            identify_with_argv("/usr/bin/python3.12", &["python3.12", "-c", "codex"]),
            None
        );
        assert_eq!(
            identify_with_argv("/usr/bin/python3.12", &["python3.12", "-m", "codex"]),
            None
        );
        assert_eq!(
            identify_with_argv("/bin/bash", &["bash", "-c", "codex --help"]),
            None
        );
        // But a shell running an agent script identifies.
        assert_eq!(
            identify_with_argv("/bin/bash", &["bash", "/opt/agents/claude"]).as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn known_package_layouts_identify_generic_entry_scripts() {
        assert_eq!(
            identify_with_argv(
                "/usr/bin/node",
                &[
                    "node",
                    "/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js"
                ]
            )
            .as_deref(),
            Some("pi")
        );
        // A random cli.js does not.
        assert_eq!(
            identify_with_argv("/usr/bin/node", &["node", "/some/tool/dist/cli.js"]),
            None
        );
    }

    #[test]
    fn extra_process_aliases_cover_names_the_manifests_lack() {
        // The npm binary is `copilot`; the manifest id is `github-copilot`
        // and its alias list assumes herdr's code-side table.
        assert_eq!(
            identify_no_argv("/opt/homebrew/bin/copilot").as_deref(),
            Some("github-copilot")
        );
    }

    #[test]
    fn versioned_launcher_paths_identify_by_parent_directory() {
        assert_eq!(
            identify_no_argv("/Users/x/.local/share/claude/versions/2.1.239").as_deref(),
            Some("claude")
        );
        // An ordinary binary under an alias-named directory (a user named
        // `pi`, say) must not be misidentified.
        assert_eq!(identify_no_argv("/home/pi/.local/bin/htop"), None);
        assert_eq!(identify_no_argv("/home/pi/.local/bin/2.0.1"), None);
        assert_eq!(
            identify_no_argv("/home/x/pi/versions/2.0.1").as_deref(),
            Some("pi")
        );
        assert_eq!(identify_no_argv("/opt/claude/deep/nested/2.1.0"), None);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_shims_identify_by_their_target() {
        let dir = std::env::temp_dir().join(format!(
            "thinkterm-proc-match-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("claude");
        std::fs::write(&target, b"#!/bin/sh\n").unwrap();
        let link = dir.join("agent-shim");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(
            identify_with_argv(
                "/usr/bin/node",
                &["node", link.to_str().unwrap()]
            )
            .as_deref(),
            Some("claude")
        );

        // Bare names never touch the filesystem.
        assert_eq!(
            identify_with_argv("/usr/bin/node", &["node", "agent-shim"]),
            None
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
