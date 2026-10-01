//! Shell integration on an ssh host, installed and removed from ThinkTerm's
//! host settings.
//!
//! Over plain ssh ThinkTerm sees no process on the far side, so a tab cannot
//! say what runs there -- unless the host's shell reports it. The shell
//! integration does: before each command it sets the `WEZTERM_PROG` user var
//! to the command line, and empties it at the prompt (see
//! `crate::foreground_program`). Setting it up puts two files in
//! `~/.config/thinkterm` over SFTP, and a helper run once adds -- or removes
//! -- the line that loads the script in the startup files the login shell
//! reads. Nothing else on the host is touched.

use anyhow::{bail, Context};
use std::io::Read;
use wezterm_ssh::Session;

const DIR: &str = ".config/thinkterm";
const SCRIPT_PATH: &str = ".config/thinkterm/shell-integration.sh";
const SETUP_PATH: &str = ".config/thinkterm/shell-integration-setup.sh";

/// The integration as installed, behind a guard: bash reads `.bashrc` and a
/// login file, and may be told to load it from both.
const SCRIPT: &str = concat!(
    "# ThinkTerm shell integration: tells the terminal which command runs.\n",
    "# Installed and removed from ThinkTerm's host settings.\n",
    "[ -n \"${__thinkterm_shell_integration-}\" ] && return 0\n",
    "__thinkterm_shell_integration=1\n",
    include_str!("../../assets/shell-integration/wezterm.sh"),
);

/// Adds or removes the loading line, marked so it can be found again, and
/// deletes itself. POSIX sh, run as `sh <path> install|remove` so the login
/// shell's own syntax never matters. It says what it did on its last line,
/// and only once every file is done -- or names the file it could not write.
const SETUP: &str = r##"# Adds or removes the line that loads ThinkTerm's shell integration in the
# startup files the login shell reads; ThinkTerm runs it over ssh from its
# host settings. It says what it did on its last line once every file is
# done. Otherwise it exits non-zero, naming a file it could not write when
# that is why, and an install leaves none of its files with the line.
script=.config/thinkterm/shell-integration.sh
marker='# thinkterm-shell-integration'
line='[ -f "$HOME/.config/thinkterm/shell-integration.sh" ] && . "$HOME/.config/thinkterm/shell-integration.sh" # thinkterm-shell-integration'
# Gone however this ends: the shell reads on from the file it has open.
rm -f "$0"
cd "$HOME" || exit 1

# Ends in failure over a file the host would not let it write, named for
# ThinkTerm on the last line: from the home as ~/..., when it is in there.
unwritable() {
  case "$1" in
    "$HOME"/*) echo "unwritable ~${1#"$HOME"}" ;;
    /*) echo "unwritable $1" ;;
    *) echo "unwritable ~/$1" ;;
  esac
  exit 1
}

shell=$(basename "${SHELL:-sh}")
if [ "$shell" = zsh ]; then
  zsh=$SHELL
else
  zsh=$(command -v zsh 2>/dev/null)
fi

# Where zsh reads .zshrc: a .zshenv may move it with ZDOTDIR. Asked of zsh
# itself, started afresh; whatever .zshenv prints comes before the answer.
zdir=
if [ -n "$zsh" ]; then
  zdir=$( (unset ZDOTDIR; exec "$zsh" -c 'printf "\n%s\n" "${ZDOTDIR:-$HOME}"') </dev/null 2>/dev/null | tail -n 1)
fi
if [ -z "$zdir" ] || [ ! -d "$zdir" ]; then
  zdir=$HOME
fi

has_line() {
  [ -f "$1" ] && grep -qF "$marker" "$1"
}

# Whether the file can be written, or made where it is missing.
writable() {
  if [ -e "$1" ]; then
    [ -w "$1" ]
  else
    [ -w "$(dirname "$1")" ]
  fi
}

add() {
  has_line "$1" && return 0
  # A file that does not end its last line would run it into ours.
  if [ -s "$1" ] && [ -n "$(tail -c 1 "$1")" ]; then
    printf '\n' >> "$1" || return 1
  fi
  printf '%s\n' "$line" >> "$1"
}

strip() {
  has_line "$1" || return 0
  kept=.config/thinkterm/kept-$$
  rm -f "$kept"
  grep -vF "$marker" "$1" > "$kept"
  copied=$?
  # 1 is "every line matched": the file held only our line. Without a whole
  # copy the file stays as it is -- written from nothing it would be empty.
  if [ ! -f "$kept" ]; then
    broken=.config/thinkterm
    return 1
  fi
  if [ "$copied" -gt 1 ]; then
    rm -f "$kept"
    broken=$1
    return 1
  fi
  # Rewritten in place, so a symlinked dotfile stays a symlink.
  broken=$1
  cat "$kept" > "$1"
  rewritten=$?
  rm -f "$kept"
  return "$rewritten"
}

# Whether a startup file still loads the script.
referenced() {
  for file in .zshrc "$zdir/.zshrc" .bashrc .bash_profile .bash_login .profile; do
    has_line "$file" && return 0
  done
  return 1
}

# Ends an install that could not write a file. The script goes too, unless
# a line from before still loads it.
give_up() {
  referenced || rm -f "$script"
  unwritable "$1"
}

case "$1" in
  install)
    case "$shell" in
      zsh)
        set -- "$zdir/.zshrc"
        ;;
      bash)
        # An interactive shell reads .bashrc; a login shell -- what
        # ThinkTerm starts over ssh -- the first of these that exists.
        login=.profile
        for candidate in .bash_profile .bash_login .profile; do
          if [ -f "$candidate" ]; then
            login=$candidate
            break
          fi
        done
        set -- .bashrc "$login"
        ;;
      *)
        rm -f "$script"
        echo "unsupported $shell"
        exit 0
        ;;
    esac
    # Nothing is touched unless every file can take the line.
    for file in "$@"; do
      writable "$file" || give_up "$file"
    done
    for file in "$@"; do
      add "$file" && continue
      # Half done: none of these files keeps the line.
      for added in "$@"; do
        strip "$added"
      done
      give_up "$file"
    done
    echo "installed $shell"
    ;;
  remove)
    left=
    for file in .zshrc "$zdir/.zshrc" .bashrc .bash_profile .bash_login .profile; do
      strip "$file" || left=${left:-$broken}
    done
    # A line that could not be taken out loads nothing without the script.
    rm -f "$script"
    [ -z "$left" ] || unwritable "$left"
    echo removed
    ;;
  *)
    echo "usage: sh $0 install|remove" >&2
    exit 1
    ;;
esac
"##;

/// What setting up a host came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellIntegrationOutcome {
    /// Loaded from now on by this login shell, from its next start.
    Installed(String),
    Removed,
    /// A login shell the integration has no hooks for (fish, say); nothing
    /// was left on the host.
    Unsupported(String),
}

/// A startup file the host would not let the setup write, from the home as
/// `~/...` when it is in there. What fails a setup most often, and what the
/// user can do something about, so it is told apart from other errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot write {0} on the host")]
pub struct UnwritableStartupFile(pub String);

/// The session to the host never came up, so the setup never ran. Not a
/// failure of the setup: the change waits for a connection that does come
/// up, and the terminal already says why this one did not.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not connected: {0}")]
pub struct NotConnected(pub String);

/// Install (`true`) or remove the integration on the host `session` is
/// connected to.
pub async fn set_up(session: &Session, install: bool) -> anyhow::Result<ShellIntegrationOutcome> {
    let sftp = session.sftp();
    for dir in [".config", DIR] {
        // There already, as often as not; a real failure shows at the write.
        let _ = sftp.create_dir(dir.to_string(), 0o755).await;
    }
    if install {
        upload(&sftp, SCRIPT_PATH, SCRIPT).await?;
    }
    upload(&sftp, SETUP_PATH, SETUP).await?;
    drop(sftp);

    let action = if install { "install" } else { "remove" };
    // Its stdin stays open below, so nothing in it may wait on input.
    let exec = session
        .exec(&format!("sh {SETUP_PATH} {action} </dev/null"), None)
        .await
        .context("running the setup")?;
    // Every stream stays open until both outputs are read: dropping stdin
    // closes the whole channel.
    let wezterm_ssh::ExecResult {
        stdin,
        mut stdout,
        mut stderr,
        mut child,
    } = exec;
    let (output, errors) = smol::unblock(move || {
        let mut output = String::new();
        let mut errors = String::new();
        let _ = stdout.read_to_string(&mut output);
        let _ = stderr.read_to_string(&mut errors);
        (output, errors)
    })
    .await;
    let status = child.async_wait().await.context("waiting for the setup")?;
    drop(stdin);
    match parse_report(&output) {
        // A connection lost on the way reads as a failed exit too.
        Some(Report::Done(outcome)) if status.success() => Ok(outcome),
        Some(Report::Unwritable(path)) => Err(UnwritableStartupFile(path).into()),
        _ => {
            let errors = errors.split_whitespace().collect::<Vec<_>>().join(" ");
            if errors.is_empty() {
                bail!("the setup on the host failed: {status}");
            }
            bail!("the setup on the host failed: {status}: {errors}");
        }
    }
}

async fn upload(sftp: &wezterm_ssh::Sftp, path: &str, content: &str) -> anyhow::Result<()> {
    use smol::io::AsyncWriteExt;
    let mut file = sftp
        .create(path.to_string())
        .await
        .with_context(|| format!("creating ~/{path}"))?;
    file.write_all(content.as_bytes())
        .await
        .with_context(|| format!("writing ~/{path}"))?;
    file.flush()
        .await
        .with_context(|| format!("writing ~/{path}"))?;
    Ok(())
}

/// What the helper says on its last line.
#[derive(Debug, PartialEq, Eq)]
enum Report {
    Done(ShellIntegrationOutcome),
    Unwritable(String),
}

fn parse_report(output: &str) -> Option<Report> {
    // The last line: the login shell that runs the helper may print first,
    // from a .zshenv say.
    let line = output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())?
        .trim();
    if line == "removed" {
        return Some(Report::Done(ShellIntegrationOutcome::Removed));
    }
    let (word, rest) = line.split_once(' ')?;
    let rest = rest.trim().to_string();
    match word {
        "installed" => Some(Report::Done(ShellIntegrationOutcome::Installed(rest))),
        "unsupported" => Some(Report::Done(ShellIntegrationOutcome::Unsupported(rest))),
        "unwritable" => Some(Report::Unwritable(rest)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_are_read_from_the_last_line() {
        let done = |outcome| Some(Report::Done(outcome));
        assert_eq!(
            parse_report("installed bash\n"),
            done(ShellIntegrationOutcome::Installed("bash".to_string()))
        );
        assert_eq!(
            parse_report("removed\n"),
            done(ShellIntegrationOutcome::Removed)
        );
        assert_eq!(
            parse_report("unsupported fish\n"),
            done(ShellIntegrationOutcome::Unsupported("fish".to_string()))
        );
        assert_eq!(
            parse_report("unwritable ~/.config/zsh/.zshrc\n"),
            Some(Report::Unwritable("~/.config/zsh/.zshrc".to_string()))
        );
        // Whatever the login shell printed before the helper ran.
        assert_eq!(
            parse_report("welcome back\ninstalled zsh\n\n"),
            done(ShellIntegrationOutcome::Installed("zsh".to_string()))
        );
        assert_eq!(parse_report("sh: not found\n"), None);
        assert_eq!(parse_report("installed\n"), None);
        assert_eq!(parse_report(""), None);
    }

    /// An empty home of its own for one test, removed when dropped.
    #[cfg(unix)]
    struct ScratchHome(std::path::PathBuf);

    #[cfg(unix)]
    impl ScratchHome {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "thinkterm-shell-integration-{}-{name}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    #[cfg(unix)]
    impl Drop for ScratchHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The shells a host may run the helper with as `sh`: bash goes by that
    /// name on some systems and dash on others, and a failed redirection
    /// ends with a different status in each. Those installed here.
    #[cfg(unix)]
    fn posix_shells() -> Vec<&'static str> {
        ["/bin/sh", "/bin/dash"]
            .iter()
            .copied()
            .filter(|path| std::path::Path::new(path).exists())
            .collect()
    }

    /// Puts the helper in a scratch home and runs it there with `sh`, as the
    /// host would.
    #[cfg(unix)]
    fn run_setup(sh: &str, home: &std::path::Path, shell: &str, action: &str) -> (bool, String) {
        std::fs::create_dir_all(home.join(DIR)).unwrap();
        std::fs::write(home.join(SETUP_PATH), SETUP).unwrap();
        run_placed_setup(sh, home, shell, action)
    }

    /// Runs the helper already in place: whether it ended well, and its
    /// stdout.
    #[cfg(unix)]
    fn run_placed_setup(
        sh: &str,
        home: &std::path::Path,
        shell: &str,
        action: &str,
    ) -> (bool, String) {
        let output = std::process::Command::new(sh)
            .arg(SETUP_PATH)
            .arg(action)
            .current_dir(home)
            .env("HOME", home)
            .env("SHELL", shell)
            .env_remove("ZDOTDIR")
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8(output.stdout).unwrap().trim().to_string(),
        )
    }

    /// The last line a shell prints when started as a login over ssh starts
    /// it, in a scratch home, and asked to run `command`.
    #[cfg(unix)]
    fn started_shell_says(home: &std::path::Path, shell: &str, command: &str) -> String {
        let output = std::process::Command::new(shell)
            .args(["-l", "-i", "-c", command])
            .current_dir(home)
            .env("HOME", home)
            .env("TERM", "xterm-256color")
            .env_remove("ZDOTDIR")
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .env_remove("TERM_PROGRAM")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .last()
            .unwrap_or_default()
            .to_string()
    }

    #[cfg(unix)]
    fn set_mode(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Whether permissions stop this process from writing `path`. They do
    /// not stop root, and a test of a file that cannot be written means
    /// nothing there.
    #[cfg(unix)]
    fn held_back_by(path: &std::path::Path) -> bool {
        if path.is_dir() {
            let probe = path.join("probe");
            let made = std::fs::write(&probe, "").is_ok();
            let _ = std::fs::remove_file(&probe);
            !made
        } else {
            std::fs::OpenOptions::new().append(true).open(path).is_err()
        }
    }

    #[cfg(unix)]
    fn marked_lines(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("# thinkterm-shell-integration"))
            .count()
    }

    #[cfg(unix)]
    #[test]
    fn bash_gets_its_rc_and_login_file_once_and_loses_them_again() {
        for (n, sh) in posix_shells().into_iter().enumerate() {
            let scratch = ScratchHome::new(&format!("bash-{n}"));
            let home = scratch.0.as_path();
            // A login file with no final newline, which must stay intact.
            std::fs::write(home.join(".bash_profile"), "export EDITOR=vi").unwrap();

            let installed = (true, "installed bash".to_string());
            assert_eq!(
                run_setup(sh, home, "/bin/bash", "install"),
                installed,
                "{sh}"
            );
            // Again: nothing doubles.
            assert_eq!(
                run_setup(sh, home, "/bin/bash", "install"),
                installed,
                "{sh}"
            );
            assert_eq!(marked_lines(&home.join(".bashrc")), 1, "{sh}");
            assert_eq!(marked_lines(&home.join(".bash_profile")), 1, "{sh}");
            let login = std::fs::read_to_string(home.join(".bash_profile")).unwrap();
            assert!(login.starts_with("export EDITOR=vi\n"), "{}", sh);
            // .bash_profile shadows .profile, which is left alone.
            assert!(!home.join(".profile").exists(), "{}", sh);
            // The helper deletes itself.
            assert!(!home.join(SETUP_PATH).exists(), "{}", sh);

            let removed = (true, "removed".to_string());
            assert_eq!(run_setup(sh, home, "/bin/bash", "remove"), removed, "{sh}");
            assert_eq!(marked_lines(&home.join(".bashrc")), 0, "{sh}");
            assert_eq!(
                std::fs::read_to_string(home.join(".bash_profile")).unwrap(),
                "export EDITOR=vi\n",
                "{sh}"
            );
            assert!(!home.join(SETUP_PATH).exists(), "{}", sh);
        }
    }

    #[cfg(unix)]
    #[test]
    fn zsh_gets_zshrc_and_other_shells_are_left_alone() {
        for (n, sh) in posix_shells().into_iter().enumerate() {
            let scratch = ScratchHome::new(&format!("zsh-{n}"));
            let home = scratch.0.as_path();
            let installed = (true, "installed zsh".to_string());
            assert_eq!(
                run_setup(sh, home, "/usr/bin/zsh", "install"),
                installed,
                "{sh}"
            );
            assert_eq!(marked_lines(&home.join(".zshrc")), 1, "{sh}");
            assert!(!home.join(".bashrc").exists(), "{}", sh);
            let removed = (true, "removed".to_string());
            assert_eq!(
                run_setup(sh, home, "/usr/bin/zsh", "remove"),
                removed,
                "{sh}"
            );
            assert_eq!(marked_lines(&home.join(".zshrc")), 0, "{sh}");

            std::fs::write(home.join(SCRIPT_PATH), "x").unwrap();
            let unsupported = (true, "unsupported fish".to_string());
            assert_eq!(
                run_setup(sh, home, "/usr/bin/fish", "install"),
                unsupported,
                "{sh}"
            );
            assert!(!home.join(".config/fish").exists(), "{}", sh);
            assert!(!home.join(SCRIPT_PATH).exists(), "{}", sh);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_cannot_be_written_fails_the_install_and_touches_nothing() {
        for (n, sh) in posix_shells().into_iter().enumerate() {
            let scratch = ScratchHome::new(&format!("read-only-{n}"));
            let home = scratch.0.as_path();
            let login = home.join(".bash_profile");
            std::fs::write(&login, "export EDITOR=vi\n").unwrap();
            set_mode(&login, 0o444);
            if !held_back_by(&login) {
                return;
            }
            std::fs::create_dir_all(home.join(DIR)).unwrap();
            std::fs::write(home.join(SCRIPT_PATH), SCRIPT).unwrap();

            // It names the file, and not even .bashrc, which could be
            // written, takes the line.
            assert_eq!(
                run_setup(sh, home, "/bin/bash", "install"),
                (false, "unwritable ~/.bash_profile".to_string()),
                "{sh}"
            );
            assert!(!home.join(".bashrc").exists(), "{}", sh);
            assert_eq!(
                std::fs::read_to_string(&login).unwrap(),
                "export EDITOR=vi\n",
                "{sh}"
            );
            assert!(!home.join(SCRIPT_PATH).exists(), "{}", sh);
            assert!(!home.join(SETUP_PATH).exists(), "{}", sh);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_file_is_never_emptied_when_it_cannot_be_copied() {
        for (n, sh) in posix_shells().into_iter().enumerate() {
            let scratch = ScratchHome::new(&format!("no-copy-{n}"));
            let home = scratch.0.as_path();
            std::fs::write(home.join(".bashrc"), "export KEEP=me\n").unwrap();
            assert!(run_setup(sh, home, "/bin/bash", "install").0, "{}", sh);
            let before = std::fs::read_to_string(home.join(".bashrc")).unwrap();

            // The copy is made beside the helper, in a folder that now takes
            // no new file.
            std::fs::write(home.join(SETUP_PATH), SETUP).unwrap();
            let dir = home.join(DIR);
            set_mode(&dir, 0o555);
            if !held_back_by(&dir) {
                set_mode(&dir, 0o755);
                return;
            }
            let said = run_placed_setup(sh, home, "/bin/bash", "remove");
            set_mode(&dir, 0o755);
            assert_eq!(
                said,
                (false, "unwritable ~/.config/thinkterm".to_string()),
                "{sh}"
            );
            assert_eq!(
                std::fs::read_to_string(home.join(".bashrc")).unwrap(),
                before,
                "{sh}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn zsh_takes_the_line_where_its_zdotdir_points() {
        let Some(zsh) = ["/bin/zsh", "/usr/bin/zsh"]
            .iter()
            .copied()
            .find(|path| std::path::Path::new(path).exists())
        else {
            return;
        };
        for (n, sh) in posix_shells().into_iter().enumerate() {
            let scratch = ScratchHome::new(&format!("zdotdir-{n}"));
            let home = scratch.0.as_path();
            std::fs::create_dir_all(home.join(".config/zsh")).unwrap();
            // Not exported, as it is often written, and with a greeting on
            // stdout.
            std::fs::write(
                home.join(".zshenv"),
                "ZDOTDIR=\"$HOME/.config/zsh\"\necho welcome\n",
            )
            .unwrap();

            let installed = (true, "installed zsh".to_string());
            assert_eq!(run_setup(sh, home, zsh, "install"), installed, "{sh}");
            assert_eq!(marked_lines(&home.join(".config/zsh/.zshrc")), 1, "{sh}");
            assert!(!home.join(".zshrc").exists(), "{}", sh);

            std::fs::write(home.join(SCRIPT_PATH), SCRIPT).unwrap();
            let said = started_shell_says(
                home,
                zsh,
                "print -r -- \"${__thinkterm_shell_integration-}:${(j:,:)precmd_functions}\"",
            );
            assert!(
                said.starts_with("1:") && said.contains("__wezterm_user_vars_precmd"),
                "{}: {}",
                sh,
                said
            );

            let removed = (true, "removed".to_string());
            assert_eq!(run_setup(sh, home, zsh, "remove"), removed, "{sh}");
            assert_eq!(marked_lines(&home.join(".config/zsh/.zshrc")), 0, "{sh}");

            // A file there it cannot write is named from the home.
            let rc = home.join(".config/zsh/.zshrc");
            set_mode(&rc, 0o444);
            if held_back_by(&rc) {
                let unwritable = (false, "unwritable ~/.config/zsh/.zshrc".to_string());
                assert_eq!(run_setup(sh, home, zsh, "install"), unwritable, "{sh}");
            }
            set_mode(&rc, 0o644);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_bash_login_shell_loads_the_line() {
        if !std::path::Path::new("/bin/bash").exists() {
            return;
        }
        let scratch = ScratchHome::new("bash-login");
        let home = scratch.0.as_path();
        std::fs::write(home.join(".bash_profile"), "export EDITOR=vi\n").unwrap();
        assert!(run_setup("/bin/sh", home, "/bin/bash", "install").0);

        std::fs::write(home.join(SCRIPT_PATH), SCRIPT).unwrap();
        let said = started_shell_says(
            home,
            "/bin/bash",
            "echo \"${__thinkterm_shell_integration-}:${precmd_functions[*]}\"",
        );
        assert!(
            said.starts_with("1:") && said.contains("__wezterm_user_vars_precmd"),
            "{}",
            said
        );
    }

    #[test]
    fn the_integration_loads_once_per_shell() {
        assert!(SCRIPT.contains("[ -n \"${__thinkterm_shell_integration-}\" ] && return 0"));
        assert!(SCRIPT.contains("WEZTERM_PROG"));
    }
}
