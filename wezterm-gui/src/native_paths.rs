use std::path::PathBuf;

pub(crate) fn data_dir() -> PathBuf {
    dirs_next::data_dir()
        .unwrap_or_else(|| config::HOME_DIR.join(".local/share"))
        .join("ThinkTerm")
}

pub(crate) fn data_file(name: &str) -> PathBuf {
    data_dir().join(name)
}
