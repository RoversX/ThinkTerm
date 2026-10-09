//! Which shells this machine actually has, for the default-shell setting.
//!
//! Both platform tables are consulted unconditionally and the module
//! carries no `cfg`: discovery is only "read some environment variables,
//! join some paths, check what exists", and those are cross-platform
//! APIs. A Windows variable is simply absent on a mac, so that table
//! yields nothing there — and the reverse for the unix paths. Keeping it
//! that way is deliberate: the Windows candidates then compile and are
//! unit-tested on the development machine instead of only in CI, which
//! is the same reason `mux::agent_status::windows_select` keeps its core
//! pure.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A shell the user can pick. `argv` is what a pane will run; `label` is
/// what the dropdown shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveredShell {
    pub label: String,
    pub argv: Vec<String>,
}

impl DiscoveredShell {
    fn new(label: &str, program: &str) -> Self {
        Self {
            label: label.to_string(),
            argv: vec![program.to_string()],
        }
    }
}

/// Windows candidates, most conventional first. PowerShell 7 is looked up
/// by explicit version directory rather than by listing `PowerShell\*`,
/// because probing paths is all this module is allowed to do. A version
/// installed somewhere else is not offered yet -- the planned "Custom"
/// entry is what will reach it; until then, `default_shell` in
/// settings.json.
const WINDOWS_POWERSHELL_VERSIONS: &[&str] = &["7", "8"];

/// WSL distributions offered at most: the dropdown neither scrolls nor
/// flips upward, so a machine with many must not push it off the window.
const WSL_DISTRO_LIMIT: usize = 3;

/// Unix candidates, curated rather than taken from `/etc/shells`: that
/// file lists `csh`, `dash`, `ksh` and `tcsh` on a stock mac, which
/// nobody is choosing here, and the dropdown has no room to spare (its
/// menu neither scrolls nor flips upward). The login shell is offered
/// whatever it is, so an omission only bites a shell that is neither in
/// this table nor the user's own; until the "Custom" entry lands, that
/// case means `default_shell` in settings.json.
const UNIX_SHELLS: &[(&str, &str)] = &[
    ("zsh", "zsh"),
    ("bash", "bash"),
    ("fish", "fish"),
    ("nushell", "nu"),
    ("sh", "sh"),
];

/// Where unix shells live, in the order a user would expect to get: a
/// Homebrew fish should win over a system one of the same name.
const UNIX_PREFIXES: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/local/sbin",
];

/// The shells present on this machine.
pub(crate) fn discover() -> Vec<DiscoveredShell> {
    discover_with(&real_env, &is_executable_file, &real_wsl_distros())
}

/// The distributions WSL has, its default first, by the names `wsl.exe -d`
/// takes. Read from the registry, where WSL records them, rather than by
/// running `wsl.exe -l`, which Settings would have to wait on as it opens.
/// The one probe here that is not a path, so the one with a `cfg`.
#[cfg(windows)]
fn real_wsl_distros() -> Vec<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let Ok(lxss) = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Lxss")
    else {
        return vec![];
    };
    let default: Option<String> = lxss.get_value("DefaultDistribution").ok();
    let mut distros = vec![];
    for id in lxss.enum_keys().flatten() {
        let Ok(key) = lxss.open_subkey(&id) else {
            continue;
        };
        // 1 is installed; one still installing or being removed is listed
        // too, and `wsl.exe -d` would only fail on it.
        if key.get_value::<u32, _>("State").is_ok_and(|state| state != 1) {
            continue;
        }
        let Ok(name) = key.get_value::<String, _>("DistributionName") else {
            continue;
        };
        if default.as_deref() == Some(id.as_str()) {
            distros.insert(0, name);
        } else {
            distros.push(name);
        }
    }
    distros
}

#[cfg(not(windows))]
fn real_wsl_distros() -> Vec<String> {
    vec![]
}

/// `SHELL` is deliberately absent: `env_bootstrap` removes it at startup
/// because it is stale after `chsh`, so reading it here would always come
/// back empty. The login shell is resolved the way the rest of this crate
/// does it -- through the password database, via the same helper the
/// spawn path itself uses.
fn real_env(name: &str) -> Option<String> {
    if name == "SHELL" {
        let shell = portable_pty::CommandBuilder::new_default_prog().get_shell();
        return (!shell.is_empty()).then_some(shell);
    }
    std::env::var(name).ok()
}

/// A candidate has to be a file this process could actually execute.
/// `exists` is not enough: a directory, or a binary left non-executable by
/// an interrupted upgrade, would pass it and then fail at spawn time with
/// the pane never opening.
fn is_executable_file(path: &str) -> bool {
    let Ok(metadata) = Path::new(path).metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

/// The testable core. `env` reads an environment variable, `exists`
/// answers whether a path is present, and `wsl_distros` are the WSL
/// distributions installed; all are injected so the whole table can be
/// exercised for either platform from any host.
pub(crate) fn discover_with(
    env: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&str) -> bool,
    wsl_distros: &[String],
) -> Vec<DiscoveredShell> {
    let mut found = vec![];
    let mut seen = HashSet::new();

    let push = |shell: DiscoveredShell, seen: &mut HashSet<Vec<String>>, found: &mut Vec<_>| {
        // Keyed by command line, so the same shell reached through two
        // candidate paths is offered once, while the WSL distributions,
        // which share a launcher, are each offered.
        if seen.insert(shell.argv.clone()) {
            found.push(shell);
        }
    };

    // The user's current login shell first: it is the one they already
    // chose, and it is routinely absent from any fixed table (a Homebrew
    // fish, a hand-built zsh).
    if let Some(shell) = env("SHELL").filter(|shell| exists(shell)) {
        let label = file_stem(&shell).unwrap_or_else(|| shell.clone());
        push(DiscoveredShell::new(&label, &shell), &mut seen, &mut found);
    }

    for (label, program) in UNIX_SHELLS {
        for prefix in UNIX_PREFIXES {
            // Joined as text, not as a `PathBuf`: these are POSIX prefixes,
            // and on Windows `PathBuf::join` would spell `/bin/bash` as
            // `/bin\bash` -- a path nothing answers to, and one the tests
            // (which exercise the unix table from any host) never see.
            let path = format!("{prefix}/{program}");
            if exists(&path) {
                push(DiscoveredShell::new(label, &path), &mut seen, &mut found);
                break;
            }
        }
    }

    for shell in windows_candidates(env, exists, wsl_distros) {
        push(shell, &mut seen, &mut found);
    }

    found
}

fn windows_candidates(
    env: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&str) -> bool,
    wsl_distros: &[String],
) -> Vec<DiscoveredShell> {
    let mut found = vec![];

    let system_root = env("SystemRoot").unwrap_or_else(|| r"C:\Windows".to_string());

    // ComSpec is the command processor this machine actually uses; the
    // System32 path is the fallback for a stripped environment.
    let cmd = env("ComSpec").filter(|path| exists(path)).or_else(|| {
        let path = join_windows(&system_root, &["System32", "cmd.exe"]);
        exists(&path).then_some(path)
    });
    if let Some(cmd) = cmd {
        found.push(DiscoveredShell::new("Command Prompt", &cmd));
    }

    let powershell = join_windows(
        &system_root,
        &["System32", "WindowsPowerShell", "v1.0", "powershell.exe"],
    );
    if exists(&powershell) {
        found.push(DiscoveredShell::new("Windows PowerShell", &powershell));
    }

    // Newest first, so "PowerShell" means the most capable one installed.
    for program_files in ["ProgramFiles", "ProgramFiles(x86)"] {
        let Some(root) = env(program_files) else {
            continue;
        };
        for version in WINDOWS_POWERSHELL_VERSIONS.iter().rev() {
            let path = join_windows(&root, &["PowerShell", version, "pwsh.exe"]);
            if exists(&path) {
                found.push(DiscoveredShell::new(
                    &format!("PowerShell {version}"),
                    &path,
                ));
                break;
            }
        }
    }

    // The Store build installs an alias here rather than under Program
    // Files, and it is the only pwsh some machines have.
    if !found
        .iter()
        .any(|shell| shell.label.starts_with("PowerShell "))
    {
        if let Some(local) = env("LOCALAPPDATA") {
            let path = join_windows(&local, &["Microsoft", "WindowsApps", "pwsh.exe"]);
            if exists(&path) {
                found.push(DiscoveredShell::new("PowerShell", &path));
            }
        }
    }

    // Git for Windows' bash, started as its own Windows Terminal profile
    // starts it: interactive and a login shell, so the profile Git ships
    // sets up its PATH. Installed for everyone, for one user, or by Scoop.
    let git_roots = [
        env("ProgramFiles").map(|root| join_windows(&root, &["Git"])),
        env("ProgramFiles(x86)").map(|root| join_windows(&root, &["Git"])),
        env("LOCALAPPDATA").map(|root| join_windows(&root, &["Programs", "Git"])),
        env("USERPROFILE").map(|root| join_windows(&root, &["scoop", "apps", "git", "current"])),
    ];
    if let Some(bash) = git_roots
        .iter()
        .flatten()
        .map(|root| join_windows(root, &["bin", "bash.exe"]))
        .find(|path| exists(path))
    {
        found.push(DiscoveredShell {
            label: "Git Bash".to_string(),
            argv: vec![bash, "-i".to_string(), "-l".to_string()],
        });
    }

    // Each distribution by name, and only with one to launch: `wsl.exe` can
    // be there with no Linux installed. Docker Desktop's own distributions
    // are its engine, not a shell anyone opens.
    let wsl = join_windows(&system_root, &["System32", "wsl.exe"]);
    if exists(&wsl) {
        for name in wsl_distros
            .iter()
            .filter(|name| !name.starts_with("docker-desktop"))
            .take(WSL_DISTRO_LIMIT)
        {
            found.push(DiscoveredShell {
                label: format!("{name} (WSL)"),
                argv: vec![wsl.clone(), "-d".to_string(), name.clone()],
            });
        }
    }

    if let Some(nu) = search_windows_path("nu.exe", env, exists) {
        found.push(DiscoveredShell::new("nushell", &nu));
    }

    found
}

fn search_windows_path(
    program: &str,
    env: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let path = env("PATH")?;
    path.split(';')
        .filter(|entry| !entry.is_empty())
        .map(|entry| join_windows(entry, &[program]))
        .find(|candidate| exists(candidate))
}

/// Joined textually rather than through `PathBuf`, so a Windows path
/// keeps its backslashes when this runs on a unix host under test.
fn join_windows(root: &str, parts: &[&str]) -> String {
    let mut path = root.trim_end_matches('\\').to_string();
    for part in parts {
        path.push('\\');
        path.push_str(part);
    }
    path
}

/// The argv to actually spawn for a stored choice, with its program
/// resolved to something the spawn path can run, or `None` when the
/// choice cannot be run at all.
///
/// A shell uninstalled since it was chosen must fall back to the platform
/// default rather than take the pane down with it: on Windows an
/// unfindable program is handed to the OS verbatim and the spawn simply
/// fails, so the terminal never opens.
///
/// A bare program name is resolved against `PATH` here rather than
/// trusted: on unix the choice travels as `SHELL`, and
/// `CommandBuilder::get_shell` checks it with `access()`, which resolves
/// a bare name against the *working directory*, not `PATH` — so an
/// unresolved `fish` would silently do nothing at all. Everything this
/// module discovers is already absolute; a bare name only reaches here
/// from a hand-edited settings file.
pub(crate) fn resolve_chosen_argv(
    argv: &[String],
    windows: bool,
    path_var: Option<String>,
    usable: &dyn Fn(&str) -> bool,
) -> Option<Vec<String>> {
    let program = argv.first()?;
    let resolved = if program.contains('/') || program.contains('\\') {
        usable(program).then(|| program.clone())?
    } else {
        let separator = if windows { ';' } else { ':' };
        path_var?
            .split(separator)
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                let mut candidate = entry.trim_end_matches(['/', '\\']).to_string();
                candidate.push(if windows { '\\' } else { '/' });
                candidate.push_str(program);
                candidate
            })
            .find(|candidate| usable(candidate))?
    };
    let mut argv = argv.to_vec();
    argv[0] = resolved;
    Some(argv)
}

/// `resolve_chosen_argv` against this machine.
pub(crate) fn resolve_chosen_argv_now(argv: &[String]) -> Option<Vec<String>> {
    resolve_chosen_argv(
        argv,
        cfg!(windows),
        std::env::var("PATH").ok(),
        &is_executable_file,
    )
}

fn file_stem(path: &str) -> Option<String> {
    Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn probe(env: &[(&str, &str)], present: &[&str]) -> (HashMap<String, String>, HashSet<String>) {
        (
            env.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            present.iter().map(|p| p.to_string()).collect(),
        )
    }

    fn discover_fake(env: &[(&str, &str)], present: &[&str]) -> Vec<DiscoveredShell> {
        discover_fake_with_wsl(env, present, &[])
    }

    fn discover_fake_with_wsl(
        env: &[(&str, &str)],
        present: &[&str],
        wsl: &[&str],
    ) -> Vec<DiscoveredShell> {
        let (env, present) = probe(env, present);
        let wsl: Vec<String> = wsl.iter().map(|name| name.to_string()).collect();
        discover_with(
            &|name| env.get(name).cloned(),
            &|path| present.contains(path),
            &wsl,
        )
    }

    #[test]
    fn git_bash_is_found_wherever_git_for_windows_put_it() {
        let shells = discover_fake(
            &[
                ("ProgramFiles", r"C:\Program Files"),
                ("LOCALAPPDATA", r"C:\Users\me\AppData\Local"),
            ],
            &[r"C:\Users\me\AppData\Local\Programs\Git\bin\bash.exe"],
        );
        assert_eq!(
            shells,
            vec![DiscoveredShell {
                label: "Git Bash".to_string(),
                argv: vec![
                    r"C:\Users\me\AppData\Local\Programs\Git\bin\bash.exe".to_string(),
                    "-i".to_string(),
                    "-l".to_string(),
                ],
            }]
        );
        // One for everyone wins over one in the user's own programs.
        let both = discover_fake(
            &[
                ("ProgramFiles", r"C:\Program Files"),
                ("LOCALAPPDATA", r"C:\Users\me\AppData\Local"),
            ],
            &[
                r"C:\Program Files\Git\bin\bash.exe",
                r"C:\Users\me\AppData\Local\Programs\Git\bin\bash.exe",
            ],
        );
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].argv[0], r"C:\Program Files\Git\bin\bash.exe");
    }

    #[test]
    fn wsl_distributions_are_offered_by_name_when_wsl_can_launch_them() {
        let wsl = r"C:\Windows\System32\wsl.exe";
        let shells = discover_fake_with_wsl(
            &[("SystemRoot", r"C:\Windows")],
            &[wsl],
            &["Ubuntu", "docker-desktop", "docker-desktop-data", "Debian", "Alpine", "Arch"],
        );
        let labels: Vec<&str> = shells.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["Ubuntu (WSL)", "Debian (WSL)", "Alpine (WSL)"]);
        assert_eq!(
            shells[1].argv,
            vec![wsl.to_string(), "-d".to_string(), "Debian".to_string()]
        );
        // No launcher, no entries, whatever the registry says.
        let none = discover_fake_with_wsl(&[("SystemRoot", r"C:\Windows")], &[], &["Ubuntu"]);
        assert!(none.is_empty(), "{none:?}");
    }

    #[test]
    fn a_windows_machine_is_discovered_from_a_unix_host() {
        // The point of the cfg-free table: this is the Windows path set,
        // exercised on whatever machine runs the tests.
        let shells = discover_fake(
            &[
                ("SystemRoot", r"C:\Windows"),
                ("ComSpec", r"C:\Windows\System32\cmd.exe"),
                ("ProgramFiles", r"C:\Program Files"),
            ],
            &[
                r"C:\Windows\System32\cmd.exe",
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                r"C:\Program Files\PowerShell\7\pwsh.exe",
            ],
        );
        let labels: Vec<&str> = shells.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Command Prompt", "Windows PowerShell", "PowerShell 7"]
        );
        assert_eq!(
            shells[2].argv,
            vec![r"C:\Program Files\PowerShell\7\pwsh.exe".to_string()]
        );
    }

    #[test]
    fn the_newest_powershell_wins_and_the_store_alias_only_fills_a_gap() {
        let with_both = discover_fake(
            &[
                ("SystemRoot", r"C:\Windows"),
                ("ProgramFiles", r"C:\Program Files"),
                ("LOCALAPPDATA", r"C:\Users\me\AppData\Local"),
            ],
            &[
                r"C:\Program Files\PowerShell\7\pwsh.exe",
                r"C:\Program Files\PowerShell\8\pwsh.exe",
                r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\pwsh.exe",
            ],
        );
        let labels: Vec<&str> = with_both.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["PowerShell 8"]);

        let store_only = discover_fake(
            &[("LOCALAPPDATA", r"C:\Users\me\AppData\Local")],
            &[r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\pwsh.exe"],
        );
        let labels: Vec<&str> = store_only.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["PowerShell"]);
    }

    #[test]
    fn nushell_is_found_on_the_windows_path() {
        let shells = discover_fake(&[("PATH", r"C:\nope;;C:\tools")], &[r"C:\tools\nu.exe"]);
        assert_eq!(
            shells,
            vec![DiscoveredShell::new("nushell", r"C:\tools\nu.exe")]
        );
    }

    #[test]
    fn a_unix_machine_prefers_homebrew_over_the_system_copy() {
        let shells = discover_fake(
            &[("SHELL", "/bin/zsh")],
            &[
                "/bin/zsh",
                "/bin/bash",
                "/opt/homebrew/bin/fish",
                "/usr/bin/fish",
            ],
        );
        let argv: Vec<&str> = shells.iter().map(|s| s.argv[0].as_str()).collect();
        // $SHELL leads; fish resolves to the Homebrew copy and appears once.
        assert_eq!(argv, ["/bin/zsh", "/bin/bash", "/opt/homebrew/bin/fish"]);
    }

    #[test]
    fn a_login_shell_outside_the_table_is_still_offered() {
        let shells = discover_fake(
            &[("SHELL", "/opt/custom/bin/elvish")],
            &["/opt/custom/bin/elvish"],
        );
        assert_eq!(
            shells,
            vec![DiscoveredShell::new("elvish", "/opt/custom/bin/elvish")]
        );
    }

    #[test]
    fn an_uninstalled_choice_stops_being_usable() {
        let present: HashSet<String> = HashSet::from(["/bin/zsh".to_string()]);
        let usable = |path: &str| present.contains(path);
        let resolve = |argv: &[String]| resolve_chosen_argv(argv, false, None, &usable);

        assert_eq!(
            resolve(&["/bin/zsh".to_string()]),
            Some(vec!["/bin/zsh".to_string()])
        );
        // Uninstalled since it was chosen: the pane must fall back rather
        // than fail to open.
        assert_eq!(
            resolve_chosen_argv(
                &[r"C:\Program Files\PowerShell\7\pwsh.exe".to_string()],
                true,
                None,
                &usable
            ),
            None
        );
        // With arguments, the program is still what gets checked, and the
        // arguments survive resolution.
        assert_eq!(
            resolve(&["/bin/removed".to_string(), "--norc".to_string()]),
            None
        );
        assert_eq!(
            resolve(&["/bin/zsh".to_string(), "--norc".to_string()]),
            Some(vec!["/bin/zsh".to_string(), "--norc".to_string()])
        );
        assert_eq!(resolve(&[]), None);
    }

    #[test]
    fn a_bare_name_is_resolved_against_path_not_left_relative() {
        // `get_shell` checks the chosen program with access(), which
        // resolves a bare name against the working directory rather than
        // PATH -- so an unresolved name would silently do nothing.
        let present: HashSet<String> = HashSet::from([
            "/opt/homebrew/bin/fish".to_string(),
            r"C:\tools\pwsh.exe".to_string(),
        ]);
        let usable = |path: &str| present.contains(path);

        assert_eq!(
            resolve_chosen_argv(
                &["fish".to_string()],
                false,
                Some("/nope:/opt/homebrew/bin".to_string()),
                &usable
            ),
            Some(vec!["/opt/homebrew/bin/fish".to_string()])
        );
        assert_eq!(
            resolve_chosen_argv(
                &["pwsh.exe".to_string()],
                true,
                Some(r"C:\nope;C:\tools\".to_string()),
                &usable
            ),
            Some(vec![r"C:\tools\pwsh.exe".to_string()])
        );
        // Not on PATH at all: fall back rather than pretend.
        assert_eq!(
            resolve_chosen_argv(
                &["nu".to_string()],
                false,
                Some("/opt/homebrew/bin".to_string()),
                &usable
            ),
            None
        );
    }

    #[test]
    fn nothing_that_is_missing_is_ever_offered() {
        // Every candidate names a path that does not exist, including a
        // $SHELL pointing at an uninstalled shell.
        let shells = discover_fake(
            &[
                ("SHELL", "/bin/removed"),
                ("ComSpec", r"C:\Windows\System32\cmd.exe"),
                ("ProgramFiles", r"C:\Program Files"),
                ("PATH", r"C:\tools"),
            ],
            &[],
        );
        assert!(shells.is_empty(), "{shells:?}");
    }
}
