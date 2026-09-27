//! Where the plugin host lives on this machine. Every client works these
//! out the same way, so they all find the one host.

use std::path::PathBuf;

/// The directory the mux keeps its own socket in (config's `RUNTIME_DIR`),
/// worked out the same way without loading config: the host is a small
/// program, and this is all it would want from it.
pub fn runtime_dir() -> PathBuf {
    match dirs_next::runtime_dir() {
        Some(dir) => dir.join("thinkterm"),
        None => home_dir().join(".local/share/thinkterm"),
    }
}

/// The GUI's data directory (thinkterm-core's `frontend_data_dir`), where
/// the plugins keep what they own, `snippets.json` among it.
pub fn data_dir() -> PathBuf {
    dirs_next::data_dir()
        .unwrap_or_else(|| home_dir().join(".local/share"))
        .join("ThinkTerm")
}

fn home_dir() -> PathBuf {
    dirs_next::home_dir().unwrap_or_default()
}

/// `base` in [`runtime_dir`], scoped to the build profile the way config's
/// `runtime_file_name` scopes the mux's files: a debug build never reaches
/// the release build's host.
fn runtime_file(base: &str) -> PathBuf {
    let name = if cfg!(debug_assertions) {
        format!("{base}-debug")
    } else {
        base.to_string()
    };
    runtime_dir().join(name)
}

pub fn socket() -> PathBuf {
    runtime_file("plugins-sock")
}

/// Held by the running host for as long as it runs: there is one at a time.
pub fn lock() -> PathBuf {
    runtime_file("plugins-lock")
}

/// What the host says while it runs; its standard error.
pub fn log() -> PathBuf {
    runtime_file("plugins-log")
}

/// The host this build ships, beside the running executable. Beside the
/// file itself, not a link to it: the command-line tools are linked into a
/// bin directory from the app bundle, and the host is not.
pub fn host_program() -> std::io::Result<PathBuf> {
    let name = if cfg!(windows) {
        "thinkterm-plugin-server.exe"
    } else {
        "thinkterm-plugin-server"
    };
    let exe = std::env::current_exe()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    Ok(exe.with_file_name(name))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_host_files_share_the_mux_runtime_dir() {
        let dir = super::runtime_dir();
        assert_eq!(dir.file_name().unwrap(), "thinkterm");
        for path in [super::socket(), super::lock(), super::log()] {
            assert_eq!(path.parent().unwrap(), dir);
        }
        assert_eq!(super::data_dir().file_name().unwrap(), "ThinkTerm");
    }
}
