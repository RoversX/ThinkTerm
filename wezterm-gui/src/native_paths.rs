use std::path::PathBuf;

pub(crate) fn data_dir() -> PathBuf {
    thinkterm_core::frontend_data_dir()
}

pub(crate) fn data_file(name: &str) -> PathBuf {
    data_dir().join(name)
}
