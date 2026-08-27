//! The process-wide "which shell should a new pane run" preference.
//!
//! The GUI owns a native setting for this; the mux is where every local
//! spawn resolves its program, so the choice is installed here as a
//! process-local override rather than plumbed through each spawn site.
//! Only the GUI installs one — a headless mux server has no such setting,
//! so a client's local preference can never decide what a remote server
//! spawns.

use parking_lot::RwLock;
use std::sync::Arc;

type Preference = Arc<dyn Fn() -> Option<Vec<String>> + Send + Sync>;

lazy_static::lazy_static! {
    static ref PREFERENCE: RwLock<Option<Preference>> = RwLock::new(None);
}

/// Install the preference. The closure is consulted on every local spawn
/// that has not been given an explicit program, so it must be cheap; the
/// GUI reads a cached settings handle.
pub fn set_process_preference(
    preference: impl Fn() -> Option<Vec<String>> + Send + Sync + 'static,
) {
    *PREFERENCE.write() = Some(Arc::new(preference));
}

/// The argv the user chose, if any. `None` means "no preference": keep the
/// platform default (`$SHELL` / `%ComSpec%`) or whatever the Lua config
/// asked for.
pub fn preferred_argv() -> Option<Vec<String>> {
    // Cloned out and the guard dropped before the closure runs: it checks
    // that the chosen shell still exists, and a stalled filesystem must
    // not be able to hold this lock -- and every later spawn -- with it.
    let preference = PREFERENCE.read().clone()?;
    let argv = preference()?;
    if argv.is_empty() {
        return None;
    }
    Some(argv)
}

/// How a chosen shell should be applied to a command that is still asking
/// for the default program.
///
/// The two arms exist because the login-shell convention is expressed
/// differently per platform, not as a style choice:
///
/// * On unix a login shell is spawned by prefixing argv0 with `-`, and
///   `CommandBuilder` only does that while the command is still flagged as
///   the default program (`is_default_prog`). Filling in argv would clear
///   that flag and silently downgrade every pane to a non-login shell —
///   no `.zprofile`, a different PATH. Pointing `SHELL` at the choice
///   instead keeps the flag, and `CommandBuilder::get_shell` already
///   prefers `SHELL`, checks it is executable, and falls back when it is
///   not.
/// * On Windows there is no login-shell concept, so the argv is written
///   directly. `ComSpec` is deliberately NOT used to express the choice:
///   it means "the command processor" to everything downstream (batch
///   files, `%ComSpec%` references), and because it lives in the
///   builder's environment it would be serialized to a remote server and
///   change what *that* machine spawns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellApplication {
    /// Set `SHELL` to this program and leave the default-prog flag alone.
    ShellEnv(String),
    /// Write this argv into the command.
    Argv(Vec<String>),
}

/// Decide how to apply `argv`. A bare program on unix travels as `SHELL`;
/// anything the user spelled out with arguments is taken literally,
/// because they asked for exactly that command line.
pub fn shell_application(argv: &[String], windows: bool) -> Option<ShellApplication> {
    let program = argv.first()?;
    if !windows && argv.len() == 1 {
        return Some(ShellApplication::ShellEnv(program.clone()));
    }
    Some(ShellApplication::Argv(argv.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_unix_shell_travels_as_the_shell_env_var() {
        // The login-shell argv0 is only applied while the command is still
        // the default program, so a bare choice must not fill in argv.
        assert_eq!(
            shell_application(&["/bin/bash".to_string()], false),
            Some(ShellApplication::ShellEnv("/bin/bash".to_string()))
        );
    }

    #[test]
    fn a_unix_choice_with_arguments_is_taken_literally() {
        let argv = vec!["/bin/bash".to_string(), "--norc".to_string()];
        assert_eq!(
            shell_application(&argv, false),
            Some(ShellApplication::Argv(argv))
        );
    }

    #[test]
    fn windows_always_writes_argv() {
        // No login shell to preserve, and ComSpec must keep meaning cmd.exe.
        assert_eq!(
            shell_application(&["pwsh.exe".to_string()], true),
            Some(ShellApplication::Argv(vec!["pwsh.exe".to_string()]))
        );
    }

    #[test]
    fn an_empty_choice_is_not_a_choice() {
        assert_eq!(shell_application(&[], false), None);
        assert_eq!(shell_application(&[], true), None);
    }

    #[test]
    fn an_empty_preference_is_treated_as_no_choice() {
        // A preference that answers "nothing" must read the same as never
        // having installed one, which is what every headless mux server is.
        let preference: Preference = Arc::new(|| None);
        assert_eq!(preference(), None);
        let empty: Preference = Arc::new(|| Some(vec![]));
        assert!(empty().is_some_and(|argv| argv.is_empty()));
    }
}
