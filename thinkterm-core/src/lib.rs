//! Frontend-neutral ThinkTerm data and connection helpers.
//!
//! Both the desktop GUI and `thinkterm tui` consume this crate.  It contains
//! no window-system or terminal-rendering code and never owns mux state.

pub mod secret;
pub mod ssh_hosts;

/// The native GUI data directory.  Keep the historical capitalisation: the
/// existing host catalog and encryption key live under `ThinkTerm`, while
/// config's protocol/runtime data uses a separate lowercase directory.
pub fn frontend_data_dir() -> std::path::PathBuf {
    dirs_next::data_dir()
        .unwrap_or_else(|| config::HOME_DIR.join(".local/share"))
        .join("ThinkTerm")
}

/// Keep credentials local without moving unrelated workspace/snippet state.
/// Publish the key and catalog together; a migration error must not create a
/// replacement key or an empty catalog in a different location.
pub fn credential_data_dir() -> anyhow::Result<std::path::PathBuf> {
    #[cfg(windows)]
    {
        let local = dirs_next::data_local_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot locate local application data"))?
            .join("ThinkTerm").join("ssh");
        config::migrate_private_files(
            &frontend_data_dir(), &local, &["secret.key", "ssh_hosts.json"],
        )?;
        return Ok(local);
    }
    #[cfg(not(windows))]
    Ok(frontend_data_dir())
}

#[cfg(test)]
mod tests {
    #[test]
    fn frontend_store_keeps_legacy_gui_capitalisation() {
        assert_eq!(
            super::frontend_data_dir().file_name(),
            Some(std::ffi::OsStr::new("ThinkTerm"))
        );
    }
}
