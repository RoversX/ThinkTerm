use anyhow::Context;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use wezterm_uds::UnixStream;

/// There's a lot more code in this windows module than I thought I would need
/// to write.  Ostensibly, we could get away with making a symlink by taking
/// the SessionName environment variable, combining it with the class name
/// and using a symlink to point to the actual path.
/// Symlinks are problematic on Windows, and the SessionName environment
/// variable may not be set.
/// It's a bit of a chore to resolve the name, and then it would be more
/// of a chore to manage the symlink.
/// What this module does is logically equivalent to the above, except
/// that it creates a piece of shared memory in the per-desktop namespace.
/// While there is a lot of code in here, it is simpler overall because
/// the naming is managed by the OS, as well as automatically removing
/// the name from the namespace when there are no more references to it.
#[cfg(windows)]
mod windows {
    use super::*;
    use std::io::Error as IoError;
    use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
    use winapi::um::memoryapi::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    };
    use winapi::um::synchapi::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
    use winapi::um::winbase::{INFINITE, WAIT_OBJECT_0};
    use winapi::um::winnt::{HANDLE, PAGE_READWRITE};

    const MAX_NAME: usize = 1024;

    /// Keeps the published name alive for the duration of the process.
    pub struct NameHolder {
        _mapping: FileMapping,
        _view: MappedView,
    }

    /// A Windows file mapping
    struct FileMapping {
        name: String,
        handle: HANDLE,
        size: usize,
    }

    impl Drop for FileMapping {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.handle) };
        }
    }

    impl FileMapping {
        /// Create a new or open an existing mapping with the specified name/size
        pub fn create(name: &str, size: usize) -> anyhow::Result<Self> {
            let wide_name = wide_string(&name);

            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null_mut(),
                    PAGE_READWRITE,
                    0,
                    size as _,
                    wide_name.as_ptr(),
                )
            };
            if handle.is_null() {
                return Err(IoError::last_os_error())
                    .with_context(|| format!("creating shared memory with name {}", name));
            }
            Ok(Self {
                name: name.to_string(),
                handle,
                size,
            })
        }

        /// Attempt to open an existing mapping
        pub fn open(name: &str, size: usize) -> anyhow::Result<Self> {
            let wide_name = wide_string(&name);

            let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide_name.as_ptr()) };
            if handle.is_null() {
                return Err(IoError::last_os_error())
                    .with_context(|| format!("creating shared memory with name {}", name));
            }
            Ok(Self {
                name: name.to_string(),
                handle,
                size,
            })
        }

        /// Map the mapping into the process address space
        pub fn map(&self) -> anyhow::Result<MappedView> {
            let buf =
                unsafe { MapViewOfFile(self.handle, FILE_MAP_ALL_ACCESS, 0, 0, self.size as _) };
            if buf.is_null() {
                return Err(IoError::last_os_error()).with_context(|| {
                    format!("mapping view of shared memory with name {}", self.name)
                });
            }
            Ok(MappedView {
                buf: buf as _,
                size: self.size,
            })
        }
    }

    /// A mutex that can be used to coordinate between processes
    struct NamedMutex {
        handle: HANDLE,
    }
    impl Drop for NamedMutex {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }

    impl NamedMutex {
        /// Create a mutex with the specified name
        pub fn new(name: &str) -> anyhow::Result<Self> {
            let wide_name = wide_string(name);
            let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, wide_name.as_ptr()) };
            if handle.is_null() {
                return Err(IoError::last_os_error())
                    .with_context(|| format!("creating mutex name {}", name));
            }
            Ok(Self { handle })
        }

        /// Acquire the mutex, and perform `func` while the mutex is held.
        /// Once `func` returns, the mutex is released.
        /// Returns the result of `func`.
        pub fn with_lock<F, T>(&self, func: F) -> anyhow::Result<T>
        where
            F: FnOnce() -> anyhow::Result<T>,
        {
            let res = unsafe { WaitForSingleObject(self.handle, INFINITE) };
            if res != WAIT_OBJECT_0 {
                return Err(IoError::last_os_error()).context("acquire mutex");
            }

            let res = func();
            unsafe { ReleaseMutex(self.handle) };
            res
        }
    }

    /// A materialized view of a mapping
    struct MappedView {
        buf: *mut u8,
        size: usize,
    }

    impl Drop for MappedView {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(self.buf as _);
            }
        }
    }

    impl MappedView {
        fn slice_mut(&mut self) -> &mut [u8] {
            unsafe { std::slice::from_raw_parts_mut(self.buf, self.size) }
        }

        fn slice(&self) -> &[u8] {
            unsafe { std::slice::from_raw_parts(self.buf, self.size) }
        }
    }

    impl NameHolder {
        /// Computes the names of the objects; they use Local scoped
        /// names so that we have one per desktop, rather than one
        /// system wide.
        /// Scoped to the build profile as well as the desktop, so that a debug
        /// GUI and the release one do not publish to a single pair of objects
        /// and leave `thinkterm cli` addressing whichever started last. See
        /// [`config::runtime_file_name`].
        fn compute_names(class_name: &str) -> (String, String) {
            let scoped = config::runtime_file_name(class_name);
            let mutex_name = format!("Local\\wezterm-sock-mutex-{}", scoped);
            let map_name = format!("Local\\wezterm-sock-{}", scoped);
            (mutex_name, map_name)
        }

        /// Publish path as the path for class_name.
        pub fn new(path: &Path, class_name: &str) -> anyhow::Result<Self> {
            let (mutex_name, map_name) = Self::compute_names(class_name);
            let path = path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("path has no file_name!?"))?
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("path is not UTF8!"))?
                .to_string();

            let mutex = NamedMutex::new(&mutex_name)?;
            mutex.with_lock(|| {
                let mapping = FileMapping::create(&map_name, MAX_NAME)?;
                let mut view = mapping.map()?;

                let target_slice = view.slice_mut();
                let len = path.len();

                target_slice[0..len].copy_from_slice(path.as_bytes());
                target_slice[len] = 0;

                log::debug!("published gui path as {}", path);

                Ok(Self {
                    _mapping: mapping,
                    _view: view,
                })
            })
        }

        /// Resolve the existing path for class_name
        pub fn resolve(class_name: &str) -> anyhow::Result<PathBuf> {
            let (mutex_name, map_name) = Self::compute_names(class_name);
            let mutex = NamedMutex::new(&mutex_name)?;
            mutex.with_lock(|| {
                let mapping = FileMapping::open(&map_name, MAX_NAME)?;
                let view = mapping.map()?;

                let source_slice = view.slice();
                let len = source_slice
                    .iter()
                    .position(|&c| c == 0)
                    .ok_or_else(|| anyhow::anyhow!("shared memory is not NUL terminated!"))?;

                let path = std::str::from_utf8(&source_slice[0..len])
                    .context("reading path from shared memory")?;

                let path: PathBuf = path.into();

                Ok(path)
            })
        }
    }

    /// Convert a rust string to a windows wide string
    fn wide_string(s: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
}

#[cfg(unix)]
mod unix {
    use super::*;

    pub struct NameHolder {
        published: PathBuf,
        name: PathBuf,
    }

    impl Drop for NameHolder {
        fn drop(&mut self) {
            // If it still points to us, remove the symlink
            if let Ok(target) = std::fs::read_link(&self.name) {
                if target == self.published {
                    log::trace!("removing {}", self.name.display());
                    std::fs::remove_file(&self.name).ok();
                }
            }
        }
    }

    impl NameHolder {
        /// Name the symlink that points `thinkterm cli` at the running GUI.
        ///
        /// Scoped to the build profile for the same reason the mux socket is
        /// (see [`config::runtime_file_name`]): a debug GUI and the release
        /// one otherwise publish to a single name, last writer wins, and CLI
        /// commands silently address whichever process started most recently
        /// rather than the one the user meant.
        fn compute_name(class_name: &str) -> String {
            config::runtime_file_name(&Self::compute_display_name(class_name))
        }

        fn compute_display_name(class_name: &str) -> String {
            #[cfg(not(target_os = "macos"))]
            {
                let config = config::configuration();
                if config.enable_wayland {
                    if let Ok(wayland) = std::env::var("WAYLAND_DISPLAY") {
                        return format!("wayland-{}-{}", wayland, class_name);
                    }
                    // We don't assume a default WAYLAND_DISPLAY here because
                    // we don't know if the default should be used or if we
                    // should fall back to X11 without connecting to wayland.
                    // We cannot introduce a dep on a wayland client library
                    // here, but we could potentially try to construct a
                    // unix domain socket client to see if our assumed default
                    // is a working unix socket.
                    // Something to fill in later as/when that question arises!
                }
                let x11 = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
                return format!("x11-{}-{}", x11, class_name);
            }
            #[cfg(target_os = "macos")]
            {
                format!("default-{}", class_name)
            }
        }

        fn compute_path(class_name: &str) -> PathBuf {
            config::RUNTIME_DIR.join(Self::compute_name(class_name))
        }

        pub fn new(path: &Path, class_name: &str) -> anyhow::Result<Self> {
            let name = Self::compute_path(class_name);
            std::fs::remove_file(&name).ok();
            std::os::unix::fs::symlink(path, &name)
                .with_context(|| format!("pointing {} -> {}", name.display(), path.display()))?;
            Ok(Self {
                published: path.to_path_buf(),
                name,
            })
        }

        pub fn resolve(class_name: &str) -> anyhow::Result<PathBuf> {
            Self::resolve_published_link(&Self::compute_path(class_name), PUBLISHED_LINK_GRACE)
        }

        /// Resolve a published name to the socket it points at, pruning the
        /// link when it names a socket that no longer answers.
        ///
        /// The link is only removed when it is older than `grace`: a GUI
        /// binds its socket before publishing, but the guard keeps us from
        /// judging a publisher that is still coming up.  Parameterized on
        /// the path and the grace period because `RUNTIME_DIR` is resolved
        /// once per process and a link created by a test is always young.
        fn resolve_published_link(name: &Path, grace: Duration) -> anyhow::Result<PathBuf> {
            let target = std::fs::read_link(name)
                .with_context(|| format!("reading symlink {}", name.display()))?;

            // read_link may hand back a relative target; probe what the
            // link actually names, not what that names relative to our cwd.
            let probe = if target.is_absolute() {
                target.clone()
            } else {
                name.parent().unwrap_or_else(|| Path::new(".")).join(&target)
            };

            if !is_sock_dead(&probe) {
                return Ok(target);
            }

            let age = std::fs::symlink_metadata(name)
                .map(|meta| meta_age(&meta))
                .unwrap_or_else(|_| Duration::from_millis(300));
            if age <= grace {
                // Too new to call dead; let the caller's own connect
                // attempt decide.
                return Ok(target);
            }

            // Only unpublish the link we just judged: a GUI may have
            // republished over it in the meantime.  Same guard Drop uses.
            if std::fs::read_link(name)
                .map(|now| now == target)
                .unwrap_or(false)
            {
                log::debug!(
                    "removing stale published name {} -> {}",
                    name.display(),
                    target.display()
                );
                std::fs::remove_file(name).ok();
            }

            anyhow::bail!(
                "{} names {}, which is not accepting connections; \
                 no GUI is published",
                name.display(),
                target.display()
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::NameHolder;
        use std::time::Duration;

        #[test]
        fn the_published_name_is_scoped_to_the_build_profile() {
            let class = "com.example.thinkterm";
            let name = NameHolder::compute_name(class);
            assert!(
                name.contains(class),
                "{} no longer identifies the class it publishes for",
                name
            );
            // Tests are built with debug_assertions, which is precisely the
            // profile that must not land on the release GUI's name.
            assert_ne!(name, NameHolder::compute_display_name(class));
        }

        // The socket file names below are a single character: sun_path is
        // capped at ~104 bytes on macOS and tempdirs there are long.

        #[test]
        fn a_published_link_to_a_listening_socket_resolves() {
            let dir = tempfile::tempdir().unwrap();
            let sock = dir.path().join("s");
            let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
            let link = dir.path().join("l");
            std::os::unix::fs::symlink(&sock, &link).unwrap();

            let resolved = NameHolder::resolve_published_link(&link, Duration::ZERO).unwrap();
            assert_eq!(resolved, sock);
            assert!(std::fs::symlink_metadata(&link).is_ok());
        }

        #[test]
        fn a_published_link_to_a_dead_socket_is_unpublished() {
            let dir = tempfile::tempdir().unwrap();
            let link = dir.path().join("l");
            std::os::unix::fs::symlink(dir.path().join("s"), &link).unwrap();
            // Ensure the link's age is measurably non-zero so a
            // second-granularity filesystem cannot round it down to the
            // ZERO grace below.
            std::thread::sleep(Duration::from_millis(20));

            assert!(NameHolder::resolve_published_link(&link, Duration::ZERO).is_err());
            assert!(
                std::fs::symlink_metadata(&link).is_err(),
                "the stale link should have been removed"
            );
        }

        #[test]
        fn a_freshly_published_link_is_given_the_benefit_of_the_doubt() {
            let dir = tempfile::tempdir().unwrap();
            let sock = dir.path().join("s");
            let link = dir.path().join("l");
            std::os::unix::fs::symlink(&sock, &link).unwrap();

            let resolved =
                NameHolder::resolve_published_link(&link, Duration::from_secs(60)).unwrap();
            assert_eq!(resolved, sock);
            assert!(std::fs::symlink_metadata(&link).is_ok());
        }

        #[test]
        fn nothing_published_is_not_a_stale_link() {
            let dir = tempfile::tempdir().unwrap();
            let missing = dir.path().join("l");
            assert!(NameHolder::resolve_published_link(&missing, Duration::ZERO).is_err());
            assert!(std::fs::symlink_metadata(&missing).is_err());
        }
    }
}

#[cfg(windows)]
pub use self::windows::NameHolder;

#[cfg(unix)]
pub use self::unix::NameHolder;

/// Unconditionally update the published path to match the provided path,
/// even if there is a running instance with a legitimate published path.
pub fn publish_gui_sock_path(path: &Path, class_name: &str) -> anyhow::Result<NameHolder> {
    NameHolder::new(path, class_name)
}

/// Resolve the last published path for `class_name`.
/// On unix, a published name whose socket has stopped answering is pruned
/// (after a short grace period) and reported as an error.  Success is still
/// NO guarantee that the returned path references a running instance: the
/// instance can exit at any moment, and a freshly published name is given
/// the benefit of the doubt.
pub fn resolve_gui_sock_path(class_name: &str) -> anyhow::Result<PathBuf> {
    NameHolder::resolve(class_name)
}

/// This function returns a list of the gui-sock- paths in
/// the runtime dir.  These represent the locally running
/// instances of wezterm-gui.
/// The list is pruned of any entries that are not live
/// and then sorted with the eldest instance first.
pub fn discover_gui_socks() -> Vec<PathBuf> {
    let mut socks = vec![];

    #[derive(Debug)]
    struct Entry {
        path: PathBuf,
        age: Duration,
    }

    if let Ok(dir) = std::fs::read_dir(&*config::RUNTIME_DIR) {
        for entry in dir {
            if let Ok(entry) = entry {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with("gui-sock-") {
                        let path = entry.path();
                        if let Ok(meta) = entry.metadata() {
                            let age = meta_age(&meta);
                            if is_sock_dead(&path) && age > Duration::from_secs(1) {
                                let _ = std::fs::remove_file(&path);
                            } else {
                                socks.push(Entry { path, age });
                            }
                        }
                    }
                }
            }
        }
    }

    socks.sort_by(|a, b| a.age.cmp(&b.age).reverse());
    log::trace!("{:?}", socks);
    socks.into_iter().map(|e| e.path).collect()
}

fn is_sock_dead(sock: &std::path::Path) -> bool {
    UnixStream::connect(sock).is_err()
}

/// Get an idea of the age of the entry.
/// Some filesystems don't support reporting `created`,
/// so fall back on `modified`.
fn meta_age(meta: &std::fs::Metadata) -> Duration {
    let t = if let Ok(created) = meta.created() {
        created
    } else if let Ok(changed) = meta.modified() {
        changed
    } else {
        return Duration::from_millis(300);
    };
    if let Ok(d) = SystemTime::now().duration_since(t) {
        d
    } else {
        Duration::from_millis(300)
    }
}

/// A published link younger than this is left alone even when the socket it
/// names does not answer: the publisher may still be coming up.  Mirrors the
/// guard `discover_gui_socks` applies to `gui-sock-*` entries.
#[cfg(unix)]
const PUBLISHED_LINK_GRACE: Duration = Duration::from_secs(1);
