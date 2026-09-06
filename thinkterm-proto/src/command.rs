use serde::{Deserialize, Serialize};

/// A command to spawn, in a form that survives the wire.
///
/// Not `portable_pty::CommandBuilder`: that type is built out of `OsString`,
/// which has no serde impl at all on wasm, and whose wire encoding is an
/// OS-tagged enum -- a unix decoder *errors* on a windows-encoded value, so
/// the cross-OS spawn it appeared to support never worked. It also carried a
/// `#[cfg(unix)]` umask field, which under positional encoding meant unix
/// and windows peers disagreed about the field count of the same struct.
///
/// Byte strings, not `String`: on unix an argv entry, an env value or a path
/// is an arbitrary byte string, and unix->unix is the path that matters.
/// The bytes carry the sender's convention:
///   * unix: the raw `OsStr` bytes, lossless;
///   * windows: utf-8 (unpaired surrogates, which a well-formed win32
///     environment cannot contain, are replaced);
///   * wasm and anything else: utf-8.
///
/// Field order is the wire contract; varbincode is positional.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// argv, argv[0] first. Empty means "the receiver's default program",
    /// i.e. a login shell -- `CommandBuilder::is_default_prog()`.
    pub args: Vec<Vec<u8>>,

    /// The *complete* environment for the child, including what the sender
    /// inherited from its own process. The receiver replaces the child's
    /// environment with this wholesale (`as_command` does `env_clear()`
    /// first), so trimming it here would strip a remote shell down to the
    /// explicit overrides and nothing else -- no PATH, no HOME.
    pub env: Vec<EnvVar>,

    /// Working directory, if the sender chose one.
    pub cwd: Option<Vec<u8>>,

    /// unix `mode_t`; `None` from senders that have no umask concept.
    /// Carried for fidelity rather than effect: every spawn path on the
    /// receiving side runs `Config::apply_cmd_defaults`, which overwrites
    /// this with the server's own saved umask.
    pub umask: Option<u32>,

    /// Whether the pty becomes the child's controlling terminal. False when
    /// spawning across a flatpak boundary.
    pub controlling_tty: bool,

    /// Whether `cwd` is a requirement (the spawn fails if it cannot be
    /// opened) or a preference (degrades to the home directory). Carried so
    /// a server refuses out loud instead of silently substituting `$HOME`
    /// for a directory the sender named on purpose.
    pub require_cwd: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct EnvVar {
    /// The name in the sender's preferred casing. The receiver derives its
    /// own case-folded lookup key from this (lowercasing on windows), so the
    /// folded key is deliberately not on the wire: it is the one piece of
    /// the old encoding that was meaningless to a differently-OS'd peer.
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    /// True when the sender inherited this rather than setting it for this
    /// spawn. Not cosmetic: it decides which variables get forwarded to an
    /// ssh host or passed through `flatpak-spawn --env=`.
    pub is_from_base_env: bool,
}

// Derived Debug would print the environment as thousands of integers into
// trace logs; render lossy strings like CommandBuilder's Debug did.
impl std::fmt::Debug for CommandSpec {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.debug_struct("CommandSpec")
            .field(
                "args",
                &self
                    .args
                    .iter()
                    .map(|a| String::from_utf8_lossy(a))
                    .collect::<Vec<_>>(),
            )
            .field("env", &self.env)
            .field("cwd", &self.cwd.as_deref().map(String::from_utf8_lossy))
            .field("umask", &self.umask)
            .field("controlling_tty", &self.controlling_tty)
            .field("require_cwd", &self.require_cwd)
            .finish()
    }
}

impl std::fmt::Debug for EnvVar {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            fmt,
            "{}={:?}{}",
            String::from_utf8_lossy(&self.key),
            String::from_utf8_lossy(&self.value),
            if self.is_from_base_env { " (base)" } else { "" }
        )
    }
}
