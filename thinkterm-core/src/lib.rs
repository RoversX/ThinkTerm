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
