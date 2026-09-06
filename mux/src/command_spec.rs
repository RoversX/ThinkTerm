use portable_pty::CommandBuilder;
use std::ffi::{OsStr, OsString};
pub use thinkterm_proto::{CommandSpec, EnvVar};

/// Free-standing conversions rather than `From` impls: both types are
/// foreign here. `CommandSpec` belongs to thinkterm-proto, which must stay
/// buildable for wasm and so cannot see portable-pty; `CommandBuilder`
/// belongs to pty, which must not learn about the protocol. `mux` is the
/// lowest crate that already depends on both.
pub trait CommandSpecExt: Sized {
    fn from_command_builder(cmd: &CommandBuilder) -> Self;
    fn into_command_builder(self) -> CommandBuilder;
}

impl CommandSpecExt for CommandSpec {
    fn from_command_builder(cmd: &CommandBuilder) -> Self {
        Self {
            args: cmd.get_argv().iter().map(|a| os_to_bytes(a)).collect(),
            env: cmd
                .iter_env_entries()
                .map(|(key, value, is_from_base_env)| EnvVar {
                    key: os_to_bytes(key),
                    value: os_to_bytes(value),
                    is_from_base_env,
                })
                .collect(),
            cwd: cmd.get_cwd().map(|d| os_to_bytes(d)),
            umask: umask_of(cmd),
            controlling_tty: cmd.get_controlling_tty(),
            require_cwd: cmd.get_require_cwd(),
        }
    }

    fn into_command_builder(self) -> CommandBuilder {
        // `from_argv` seeds the environment from *this* process; the spec
        // carries the full environment the child is to get, so clear it.
        // An empty argv is the default-prog builder, which `from_argv`
        // reproduces exactly (`arg()` would panic on it; we never call it).
        let mut cmd =
            CommandBuilder::from_argv(self.args.iter().map(|a| bytes_to_os(a)).collect());
        cmd.env_clear();
        for EnvVar {
            key,
            value,
            is_from_base_env,
        } in &self.env
        {
            let (k, v) = (bytes_to_os(key), bytes_to_os(value));
            if *is_from_base_env {
                cmd.base_env(k, v);
            } else {
                cmd.env(k, v);
            }
        }
        if let Some(cwd) = &self.cwd {
            cmd.cwd(bytes_to_os(cwd));
        }
        apply_umask(&mut cmd, self.umask);
        cmd.set_controlling_tty(self.controlling_tty);
        cmd.set_require_cwd(self.require_cwd);
        cmd
    }
}

#[cfg(unix)]
fn os_to_bytes(s: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes().to_vec()
}
#[cfg(unix)]
fn bytes_to_os(b: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(b.to_vec())
}
#[cfg(not(unix))]
fn os_to_bytes(s: &OsStr) -> Vec<u8> {
    s.to_string_lossy().into_owned().into_bytes()
}
#[cfg(not(unix))]
fn bytes_to_os(b: &[u8]) -> OsString {
    String::from_utf8_lossy(b).into_owned().into()
}

// u32::from copes with mode_t being u16 on macOS and u32 on Linux; the cast
// back is safe for 12-bit mode values, and leb128 keeps the width off the
// wire either way.
#[cfg(unix)]
fn umask_of(cmd: &CommandBuilder) -> Option<u32> {
    cmd.get_umask().map(|m| m as u32)
}
#[cfg(not(unix))]
fn umask_of(_: &CommandBuilder) -> Option<u32> {
    None
}

#[cfg(unix)]
fn apply_umask(cmd: &mut CommandBuilder, mask: Option<u32>) {
    cmd.umask(mask.map(|m| m as libc::mode_t));
}
#[cfg(not(unix))]
fn apply_umask(_: &mut CommandBuilder, _: Option<u32>) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(cmd: &CommandBuilder) -> CommandBuilder {
        CommandSpec::from_command_builder(cmd).into_command_builder()
    }

    /// The received env map *is* the child's whole environment
    /// (`as_command` does env_clear first), and the base/extra flag decides
    /// what reaches ssh hosts and flatpak. Both must survive exactly.
    #[test]
    fn env_survives_with_provenance() {
        let mut cmd = CommandBuilder::new("prog");
        cmd.env("EXPLICIT_ONE", "1");
        cmd.env("EXPLICIT_TWO", "2");

        let back = round_trip(&cmd);

        let full: Vec<_> = cmd.iter_full_env_as_str().collect();
        let full_back: Vec<_> = back.iter_full_env_as_str().collect();
        assert_eq!(full, full_back);

        let extra: Vec<_> = back.iter_extra_env_as_str().collect();
        assert_eq!(extra, vec![("EXPLICIT_ONE", "1"), ("EXPLICIT_TWO", "2")]);

        // The inherited environment is still visible under its own key,
        // proving the receiver-side map_key re-derivation is a no-op here.
        assert_eq!(back.get_env("PATH"), cmd.get_env("PATH"));
    }

    /// Guard rail against "simplifying" the byte strings to String later:
    /// unix argv and env values are arbitrary bytes.
    #[cfg(unix)]
    #[test]
    fn non_utf8_bytes_survive() {
        use std::os::unix::ffi::OsStringExt;
        let arg = OsString::from_vec(vec![0x66, 0x80, 0x6f]);
        let mut cmd = CommandBuilder::new("prog");
        cmd.arg(&arg);
        cmd.env("WEIRD", OsString::from_vec(vec![0xff, 0x21]));

        let back = round_trip(&cmd);
        assert_eq!(back.get_argv(), cmd.get_argv());
        assert_eq!(back.get_env("WEIRD"), cmd.get_env("WEIRD"));
    }

    /// An empty argv is the only encoding of "spawn the login shell";
    /// a conversion built on CommandBuilder::new would lose it.
    #[test]
    fn default_prog_survives() {
        let cmd = CommandBuilder::new_default_prog();
        let back = round_trip(&cmd);
        assert!(back.is_default_prog());
        assert!(back.get_argv().is_empty());
    }

    /// controlling_tty is only ever false under flatpak, so daily use would
    /// never notice losing it; umask matters cross-platform.
    #[test]
    fn tty_flag_and_umask_survive() {
        let mut cmd = CommandBuilder::new("prog");
        cmd.set_controlling_tty(false);
        #[cfg(unix)]
        cmd.umask(Some(0o022));

        let back = round_trip(&cmd);
        assert!(!back.get_controlling_tty());
        #[cfg(unix)]
        assert_eq!(back.get_umask(), Some(0o022));
    }

    /// The flag is what turns a refused cwd into an error on the server
    /// instead of a silent `$HOME`; losing it re-opens that defect.
    #[test]
    fn require_cwd_survives() {
        let mut cmd = CommandBuilder::new("prog");
        cmd.cwd("/some/dir");
        cmd.set_require_cwd(true);
        assert!(round_trip(&cmd).get_require_cwd());
        assert!(!round_trip(&CommandBuilder::new("prog")).get_require_cwd());
    }

    /// cwd survives byte-for-byte.
    #[test]
    fn cwd_survives() {
        let mut cmd = CommandBuilder::new("prog");
        cmd.cwd("/some/dir");
        let back = round_trip(&cmd);
        assert_eq!(back.get_cwd(), cmd.get_cwd());
    }
}
