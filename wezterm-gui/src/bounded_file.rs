//! Bounded reads of untrusted preview content, including special-file rejection.
use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Component, Path};

#[derive(Debug)]
pub(crate) struct ByteBudgetExceeded;
impl std::fmt::Display for ByteBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("preview source exceeds the byte budget")
    }
}
impl std::error::Error for ByteBudgetExceeded {}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Opening a FIFO must not block before we can inspect the handle.
        options.custom_flags(libc::O_NONBLOCK);
    }
    options
}

/// Account payload bytes even if a later read fails. Opening/stat failures
/// consume no payload; callers separately budget attempts.
struct CountedReader<'a, R> {
    inner: R,
    consumed: &'a mut usize,
}
impl<R: Read> Read for CountedReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        *self.consumed = self.consumed.saturating_add(read);
        Ok(read)
    }
}

fn read_handle(file: File, limit: usize, consumed: &mut usize) -> Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("preview source is not a regular file");
    }
    if metadata.len() > limit as u64 {
        return Err(ByteBudgetExceeded.into());
    }
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(limit));
    CountedReader {
        inner: file,
        consumed,
    }
    .take(limit as u64 + 1)
    .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(ByteBudgetExceeded.into());
    }
    Ok(bytes)
}

pub(crate) fn read_regular(path: &Path, limit: usize) -> Result<Vec<u8>> {
    read_handle(
        options()
            .open(path)
            .with_context(|| format!("open {}", path.display()))?,
        limit,
        &mut 0,
    )
}

/// Resolve from an opened root without following descendant symlinks. Vault
/// indexing also excludes symlinks; a replacement between indexing and reading
/// must not turn a note into a read outside the vault.
pub(crate) fn read_in_root(
    root: &Path,
    relative: &Path,
    limit: usize,
    consumed: &mut usize,
) -> Result<Vec<u8>> {
    let parts: Vec<_> = relative
        .components()
        .filter(|c| *c != Component::CurDir)
        .collect();
    if parts.is_empty() || parts.iter().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("invalid embedded note path");
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        let mut directory = File::open(root)?;
        for (index, component) in parts.iter().enumerate() {
            let name = CString::new(component.as_os_str().as_bytes())?;
            let last = index + 1 == parts.len();
            let flags = libc::O_RDONLY
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if last { 0 } else { libc::O_DIRECTORY };
            // SAFETY: the directory fd and NUL-terminated component live through
            // openat; each returned descriptor is immediately owned by File.
            let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            if last {
                return read_handle(file, limit, consumed);
            }
            directory = file;
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
        use winapi::um::fileapi::GetFinalPathNameByHandleW;
        fn opened_path(file: &File) -> Result<std::path::PathBuf> {
            let mut buffer = vec![0u16; 512];
            loop {
                // Query the opened object, not a path that could be swapped
                // after validation. The handle and output buffer remain live.
                let length = unsafe {
                    GetFinalPathNameByHandleW(
                        file.as_raw_handle() as _,
                        buffer.as_mut_ptr(),
                        buffer.len() as u32,
                        0,
                    )
                };
                if length == 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                if (length as usize) < buffer.len() {
                    buffer.truncate(length as usize);
                    return Ok(std::ffi::OsString::from_wide(&buffer).into());
                }
                buffer.resize(length as usize + 1, 0);
            }
        }
        use winapi::um::winbase::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT};
        use winapi::um::winnt::{FILE_ATTRIBUTE_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE};
        let mut path = root.canonicalize()?;
        // Omit FILE_SHARE_DELETE: held directories cannot be renamed/replaced
        // while resolving the next component. Inspect reparse points themselves.
        let open = |path: &Path| -> Result<File> {
            let file = OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(path)?;
            if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                bail!("embedded note path contains a reparse point");
            }
            Ok(file)
        };
        let mut held = vec![open(&path)?];
        let opened_root = opened_path(&held[0])?;
        for (index, component) in parts.iter().enumerate() {
            path.push(component.as_os_str());
            let file = open(&path)?;
            // FILE_SHARE_DELETE protects renames, but cannot by itself stop
            // another writer setting a junction on an existing directory.
            if !opened_path(&file)?.starts_with(&opened_root) {
                bail!("embedded note escaped its root");
            }
            if index + 1 == parts.len() {
                return read_handle(file, limit, consumed);
            }
            if !file.metadata()?.is_dir() {
                bail!("embedded note parent is not a directory");
            }
            held.push(file);
        }
    }
    bail!("unsupported embedded note path")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn regular_files_respect_the_exact_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        std::fs::write(&path, b"abcd").unwrap();
        assert_eq!(read_regular(&path, 4).unwrap(), b"abcd");
        assert!(read_regular(&path, 3).is_err());
        assert!(read_regular(dir.path(), 100).is_err());
        assert_eq!(
            read_in_root(dir.path(), Path::new("note.md"), 4, &mut 0).unwrap(),
            b"abcd"
        );
        assert!(read_in_root(dir.path(), &path, 4, &mut 0).is_err());
        assert!(read_in_root(dir.path(), Path::new("../note.md"), 4, &mut 0).is_err());
    }
    #[test]
    fn actual_bytes_are_charged_and_preflight_errors_are_free() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.md"), b"abcd").unwrap();
        for path in ["missing.md", "..", "note.md"] {
            let mut consumed = 0;
            assert!(read_in_root(dir.path(), Path::new(path), 3, &mut consumed).is_err());
            assert_eq!(consumed, 0);
        }
        let mut consumed = 0;
        assert_eq!(
            read_in_root(dir.path(), Path::new("note.md"), 4, &mut consumed).unwrap(),
            b"abcd"
        );
        assert_eq!(consumed, 4);
        struct FailAfterFirst(bool);
        impl Read for FailAfterFirst {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 {
                    return Err(std::io::Error::other("test partial read failure"));
                }
                self.0 = true;
                buf[0] = b'x';
                Ok(1)
            }
        }
        let mut consumed = 0;
        let mut bytes = Vec::new();
        assert!(CountedReader {
            inner: FailAfterFirst(false),
            consumed: &mut consumed
        }
        .take(16)
        .read_to_end(&mut bytes)
        .is_err());
        assert_eq!(consumed, 1);
    }

    #[cfg(unix)]
    #[test]
    fn devices_fifos_and_swapped_directory_symlinks_are_rejected() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo.png");
        let cpath = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
        assert!(read_regular(&fifo, 32).is_err());
        let device = dir.path().join("device.png");
        symlink("/dev/zero", &device).unwrap();
        assert!(read_regular(&device, 32).is_err());
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("note.md"), "outside").unwrap();
        symlink(outside.path(), dir.path().join("swapped")).unwrap();
        assert!(read_in_root(dir.path(), Path::new("swapped/note.md"), 100, &mut 0).is_err());
    }
}
