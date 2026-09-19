use crate::allocate::*;
use crate::osc::{base64_decode, base64_encode};
use core::fmt::{Display, Error as FmtError, Formatter};

fn get<'a>(keys: &BTreeMap<&str, &'a str>, k: &str) -> Option<&'a str> {
    keys.get(k).map(|&s| s)
}

fn geti<T: core::str::FromStr>(keys: &BTreeMap<&str, &str>, k: &str) -> Option<T> {
    get(keys, k).and_then(|s| s.parse().ok())
}

fn set<T: ToString>(keys: &mut BTreeMap<&'static str, String>, k: &'static str, v: &Option<T>) {
    if let Some(v) = v {
        keys.insert(k, v.to_string());
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum KittyImageData {
    /// The data bytes, baes64-encoded fragments.
    /// t='d'
    Direct(String),
    DirectBin(Vec<u8>),
    /// The path to a file containing the data.
    /// t='f'
    File {
        path: String,
        /// the amount of data to read.
        /// S=...
        data_size: Option<u32>,
        /// The offset at which to read.
        /// O=...
        data_offset: Option<u32>,
    },
    /// The path to a temporary file containing the data.
    /// If the path is in a known temporary location,
    /// it should be removed once the data has been read
    /// t='t'
    TemporaryFile {
        path: String,
        /// the amount of data to read.
        /// S=...
        data_size: Option<u32>,
        /// The offset at which to read.
        /// O=...
        data_offset: Option<u32>,
    },

    /// The name of a shared memory object.
    /// Can be opened via shm_open() and then should be removed
    /// via shm_unlink().
    /// On Windows, OpenFileMapping(), MapViewOfFile(), UnmapViewOfFile()
    /// and CloseHandle() are used to access and release the data.
    /// t='s'
    SharedMem {
        name: String,
        /// the amount of data to read.
        /// S=...
        data_size: Option<u32>,
        /// The offset at which to read.
        /// O=...
        data_offset: Option<u32>,
    },
    /// An external source was read before entering the terminal lock and the
    /// read failed. Keep the error with the action so the terminal can report
    /// it without retrying a destructive TemporaryFile/SharedMem read while
    /// holding that lock.
    #[cfg(feature = "kitty-shm")]
    MaterializedError {
        kind: std::io::ErrorKind,
        message: String,
    },
}

impl core::fmt::Debug for KittyImageData {
    fn fmt(&self, fmt: &mut Formatter) -> core::fmt::Result {
        match self {
            Self::Direct(data) => write!(fmt, "Direct({} bytes of data)", data.len()),
            Self::DirectBin(data) => write!(fmt, "DirectBin({} bytes of data)", data.len()),
            Self::File {
                path,
                data_offset,
                data_size,
            } => fmt
                .debug_struct("File")
                .field("path", &path)
                .field("data_offset", &data_offset)
                .field("data_size", data_size)
                .finish(),
            Self::TemporaryFile {
                path,
                data_offset,
                data_size,
            } => fmt
                .debug_struct("TemporaryFile")
                .field("path", &path)
                .field("data_offset", &data_offset)
                .field("data_size", data_size)
                .finish(),
            Self::SharedMem {
                name,
                data_offset,
                data_size,
            } => fmt
                .debug_struct("SharedMem")
                .field("name", &name)
                .field("data_offset", &data_offset)
                .field("data_size", data_size)
                .finish(),
            #[cfg(feature = "kitty-shm")]
            Self::MaterializedError { kind, message } => fmt
                .debug_struct("MaterializedError")
                .field("kind", kind)
                .field("message", message)
                .finish(),
        }
    }
}

impl KittyImageData {
    fn from_keys(keys: &BTreeMap<&str, &str>, payload: &[u8]) -> Option<Self> {
        let t = get(keys, "t").unwrap_or("d");

        match t {
            "d" => Some(Self::Direct(String::from_utf8(payload.to_vec()).ok()?)),
            "f" => Some(Self::File {
                path: String::from_utf8(base64_decode(payload.to_vec()).ok()?).ok()?,
                data_size: geti(keys, "S"),
                data_offset: geti(keys, "O"),
            }),
            "t" => Some(Self::TemporaryFile {
                path: String::from_utf8(base64_decode(payload.to_vec()).ok()?).ok()?,
                data_size: geti(keys, "S"),
                data_offset: geti(keys, "O"),
            }),
            "s" => Some(Self::SharedMem {
                name: String::from_utf8(base64_decode(payload.to_vec()).ok()?).ok()?,
                data_size: geti(keys, "S"),
                data_offset: geti(keys, "O"),
            }),
            _ => None,
        }
    }

    fn to_keys(&self, keys: &mut BTreeMap<&'static str, String>) {
        match self {
            Self::Direct(d) => {
                keys.insert("payload", d.to_string());
            }
            Self::DirectBin(d) => {
                keys.insert("payload", base64_encode(d));
            }
            Self::File {
                path,
                data_offset,
                data_size,
            } => {
                keys.insert("t", "f".to_string());
                keys.insert("payload", base64_encode(&path));
                set(keys, "S", data_size);
                set(keys, "S", data_offset);
            }
            Self::TemporaryFile {
                path,
                data_offset,
                data_size,
            } => {
                keys.insert("t", "t".to_string());
                keys.insert("payload", base64_encode(&path));
                set(keys, "S", data_size);
                set(keys, "S", data_offset);
            }
            Self::SharedMem {
                name,
                data_offset,
                data_size,
            } => {
                keys.insert("t", "s".to_string());
                keys.insert("payload", base64_encode(&name));
                set(keys, "S", data_size);
                set(keys, "S", data_offset);
            }
            #[cfg(feature = "kitty-shm")]
            Self::MaterializedError { .. } => {
                // This internal variant has no wire representation. An empty
                // direct payload keeps diagnostic Display impls well-formed.
                keys.insert("payload", String::new());
            }
        }
    }

    /// How much memory this fragment occupies while it waits in a chunked
    /// transfer. Only the direct forms carry payload bytes; the others hold a
    /// path or object name that is not read until the transfer completes.
    /// `load_data` consumes `self` and touches the filesystem, so it cannot be
    /// used to measure a fragment that is still being accumulated.
    pub fn in_memory_len(&self) -> usize {
        match self {
            Self::Direct(data) => data.len(),
            Self::DirectBin(data) => data.len(),
            Self::File { path, .. } | Self::TemporaryFile { path, .. } => path.len(),
            Self::SharedMem { name, .. } => name.len(),
            #[cfg(feature = "kitty-shm")]
            Self::MaterializedError { message, .. } => message.len(),
        }
    }

    /// Resolve external payloads before taking the terminal-model lock.
    /// Direct data stays encoded because it may participate in the terminal's
    /// chunk accumulation rules.
    #[cfg(feature = "kitty-shm")]
    pub fn materialize_external_source(&mut self) {
        if !matches!(
            self,
            Self::File { .. } | Self::TemporaryFile { .. } | Self::SharedMem { .. }
        ) {
            return;
        }

        let source = core::mem::replace(self, Self::DirectBin(Vec::new()));
        *self = match source.load_data() {
            Ok(data) => Self::DirectBin(data),
            Err(err) => Self::MaterializedError {
                kind: err.kind(),
                message: err.to_string(),
            },
        };
    }

    /// Take the image data bytes.
    /// This operation is not repeatable as some of the sources require
    /// removing the underlying file or shared memory object as part
    /// of the read operaiton.
    #[cfg(feature = "kitty-shm")]
    pub fn load_data(self) -> std::io::Result<Vec<u8>> {
        match self {
            Self::Direct(data) => base64_decode(data).or_else(|err| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("base64 decode: {err:#}"),
                ))
            }),
            Self::DirectBin(bin) => Ok(bin),
            Self::File {
                path,
                data_offset,
                data_size,
            } => read_from_file(&path, data_offset, data_size),
            Self::TemporaryFile {
                path,
                data_offset,
                data_size,
            } => {
                // Read first, but clean up no matter how the read went: an
                // early error return here (a refused oversized file included)
                // would leave the temporary file behind on disk.
                let result = read_from_file(&path, data_offset, data_size);
                remove_temporary_file(&path);
                result
            }
            Self::SharedMem {
                name,
                data_offset,
                data_size,
            } => read_shared_memory_data(&name, data_offset, data_size),
            Self::MaterializedError { kind, message } => Err(std::io::Error::new(kind, message)),
        }
    }
}

/// The most bytes a file or shared-memory transmission may hand over. These
/// payloads are raw bytes — the base64 in the escape carries only the path
/// or object name — and the largest one that could ever decode is the raw
/// RGBA of an image at the 100MB limit the terminal enforces (MAX_IMAGE_SIZE
/// in term's image.rs; move the two together). Without a cap here the escape
/// names a file and the terminal reads all of it, however large, before
/// anything downstream gets a chance to refuse it.
/// Unlink a `t=t` temporary file, but only inside a known temporary
/// directory: the path is chosen by whatever wrote the escape.
#[cfg(feature = "kitty-shm")]
fn remove_temporary_file(path: &str) {
    fn resolves_inside_temp_dir(p: &str) -> bool {
        let resolved = match std::fs::canonicalize(p) {
            Ok(resolved) => resolved,
            Err(_) => return false,
        };

        let mut roots = vec![
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/var/tmp"),
            std::path::PathBuf::from("/dev/shm"),
        ];
        if let Ok(dir) = std::env::var("TMPDIR") {
            roots.push(dir.into());
        }

        roots
            .iter()
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .any(|root| resolved.starts_with(root))
    }

    if resolves_inside_temp_dir(path) {
        if let Err(err) = std::fs::remove_file(path) {
            log::error!(
                "Unable to remove kitty image protocol temporary file {}: {:#}",
                path,
                err
            );
        }
    } else {
        log::warn!(
            "kitty image protocol temporary file {} isn't in a known \
             temporary directory; won't try to remove it",
            path
        );
    }
}

#[cfg(feature = "kitty-shm")]
impl KittyImageData {
    /// Drop an external payload without reading it. The temporary file or
    /// shared-memory object is still unlinked -- the protocol makes that the
    /// terminal's job whether or not it read the bytes -- and the action
    /// carries a `MaterializedError` so the terminal answers the client
    /// without touching the disk. Used when one flush holds more frames
    /// than are worth reading: everything but the newest is already stale.
    pub fn discard_external_source(&mut self, reason: &str) {
        match self {
            Self::File { .. } => {}
            Self::TemporaryFile { path, .. } => remove_temporary_file(path),
            Self::SharedMem { name, .. } => {
                // nix has no shm on Android; there is nothing to unlink.
                #[cfg(all(unix, not(target_os = "android")))]
                {
                    if let Err(err) = nix::sys::mman::shm_unlink(name.as_str()) {
                        log::warn!("shm_unlink {name} while discarding a stale frame: {err:#}");
                    }
                }
                #[cfg(not(all(unix, not(target_os = "android"))))]
                {
                    let _ = name;
                }
            }
            _ => return,
        }
        *self = Self::MaterializedError {
            kind: std::io::ErrorKind::Interrupted,
            message: reason.to_string(),
        };
    }

    /// Bytes held in memory by an already materialized payload.
    pub fn materialized_len(&self) -> usize {
        match self {
            Self::DirectBin(bin) => bin.len(),
            _ => 0,
        }
    }
}

/// Materialize the external kitty payloads of one flush, newest first.
/// Once `budget` bytes have been read, an older payload is discarded unread
/// if -- and only if -- a newer transmit in the same flush carries the same
/// image id (`i=`): that older picture is replaced before anything can be
/// drawn from it. A parser that fell behind a frame stream can find
/// hundreds of frame escapes in a single read; reading all of them holds
/// gigabytes for pictures the next frame overwrites, and the frames a
/// viewer will actually see are the last ones in the batch.
///
/// Anything not superseded that way -- distinct ids, id-less transmits, a
/// gallery of separate pictures, queries, animation frames -- is read even
/// past the budget: the budget bounds the waste, never the protocol.
/// Returns (bytes materialized, actions discarded).
#[cfg(feature = "kitty-shm")]
pub fn materialize_kitty_actions_newest_first(
    actions: &mut [crate::Action],
    budget: usize,
) -> (usize, usize) {
    let mut remaining = budget;
    let mut materialized = 0usize;
    let mut discarded = 0usize;
    let mut newer_ids = std::collections::HashSet::new();
    for action in actions.iter_mut().rev() {
        let crate::Action::KittyImage(image) = action else {
            continue;
        };
        let replaces_id = image.replacing_transmit_image_id();
        if image.has_external_data_source() {
            let superseded = replaces_id.map_or(false, |id| newer_ids.contains(&id));
            if remaining == 0 && superseded {
                image.discard_data_sources("superseded by a newer frame in the same batch");
                discarded += 1;
            } else {
                image.materialize_data_sources();
                let len = image.materialized_len();
                materialized += len;
                remaining = remaining.saturating_sub(len);
            }
        }
        // Recorded after the check so a transmit never supersedes itself,
        // and for direct payloads too: a newer inline transmit replaces an
        // older file transmit of the same id just the same.
        if let Some(id) = replaces_id {
            newer_ids.insert(id);
        }
    }
    (materialized, discarded)
}

#[cfg(not(feature = "kitty-shm"))]
pub fn materialize_kitty_actions_newest_first(
    _actions: &mut [crate::Action],
    _budget: usize,
) -> (usize, usize) {
    (0, 0)
}

#[cfg(feature = "kitty-shm")]
const MAX_IMAGE_DATA_BYTES: u64 = 128 * 1024 * 1024;

/// The size hint comes from the opened descriptor, never from `S=`, so it is
/// bounded by what actually exists on disk; `cap` bounds it further. A
/// smaller eager allocation is not safer, only slower: `read_to_end` grows
/// by doubling, so a 6.7MiB frame read into a smaller buffer ends up owning
/// 8 or 16MiB, and that capacity is what the image store then keeps.
#[cfg(feature = "kitty-shm")]
fn initial_read_capacity(size_hint: Option<u64>, cap: u64) -> usize {
    let size = size_hint.unwrap_or(0).min(cap);
    usize::try_from(size).unwrap_or(usize::MAX)
}

/// Opens `path` for reading, refusing anything that is not a regular
/// file. Reading a character device such as /dev/zero never ends, and
/// opening a fifo blocks until a writer appears; either one wedges the
/// pane's parser thread on a path chosen by whatever wrote the escape.
#[cfg(feature = "kitty-shm")]
fn open_regular_file(path: &str) -> std::io::Result<(std::fs::File, u64)> {
    #[cfg(unix)]
    let f = {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NONBLOCK makes opening a writer-less fifo fail instead of
        // hanging. It does not affect reads from a regular file.
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
            .open(path)?
    };
    #[cfg(not(unix))]
    let f = std::fs::File::open(path)?;

    // Checked against the descriptor we already hold, so the answer
    // cannot change between the check and the read.
    let metadata = f.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{path} is not a regular file"),
        ));
    }
    Ok((f, metadata.len()))
}

#[cfg(feature = "kitty-shm")]
fn read_from_file(
    path: &str,
    data_offset: Option<u32>,
    data_size: Option<u32>,
) -> std::io::Result<Vec<u8>> {
    read_from_file_capped(path, data_offset, data_size, MAX_IMAGE_DATA_BYTES)
}

/// The cap is a parameter so the tests can exercise it without a
/// hundred-megabyte fixture.
#[cfg(feature = "kitty-shm")]
fn read_from_file_capped(
    path: &str,
    data_offset: Option<u32>,
    data_size: Option<u32>,
    cap: u64,
) -> std::io::Result<Vec<u8>> {
    use std::io::Seek;
    let (mut f, file_len) = open_regular_file(path)?;
    let offset = u64::from(data_offset.unwrap_or(0));
    let available = file_len.saturating_sub(offset);
    if offset != 0 {
        f.seek(std::io::SeekFrom::Start(offset))?;
    }
    if let Some(len) = data_size {
        read_exactly(&mut f, len, cap, Some(available.min(u64::from(len))))
    } else {
        read_to_end_capped(&mut f, cap, Some(available))
    }
}

/// Reads to end-of-file, refusing to hold more than `cap` bytes.
#[cfg(feature = "kitty-shm")]
fn read_to_end_capped(
    f: &mut impl std::io::Read,
    cap: u64,
    size_hint: Option<u64>,
) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut res = Vec::with_capacity(initial_read_capacity(size_hint, cap));
    // Fill the hinted size exactly first: `read_to_end` doubles the buffer
    // as soon as it is full, so reading straight to EOF into an exactly
    // sized buffer would still end at twice the size. Anything the file
    // grew by since it was opened comes in a second, unbounded-by-hint read.
    if let Some(hint) = size_hint {
        let hint = hint.min(cap.saturating_add(1));
        f.by_ref().take(hint).read_to_end(&mut res)?;
        if (res.len() as u64) < hint {
            return Ok(res);
        }
    }
    let remaining = cap.saturating_add(1).saturating_sub(res.len() as u64);
    let mut tail = Vec::new();
    f.take(remaining).read_to_end(&mut tail)?;
    if !tail.is_empty() {
        res.extend_from_slice(&tail);
    }
    if res.len() as u64 > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("image data is over the {cap} byte limit"),
        ));
    }
    Ok(res)
}

/// Reads exactly `len` bytes without trusting `len` as an allocation size.
/// `S=` is chosen by the escape writer and can be 4GiB. A value over `cap` is
/// refused before allocation; the bounded capacity hint comes from the opened
/// descriptor rather than from `S=`.
#[cfg(feature = "kitty-shm")]
fn read_exactly(
    f: &mut impl std::io::Read,
    len: u32,
    cap: u64,
    size_hint: Option<u64>,
) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    if u64::from(len) > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("wanted {len} bytes of image data, over the {cap} byte limit"),
        ));
    }
    let mut res = Vec::with_capacity(initial_read_capacity(size_hint, cap));
    let got = f.take(len.into()).read_to_end(&mut res)?;
    if got != len as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("wanted {len} bytes of image data but only {got} were available"),
        ));
    }
    Ok(res)
}

#[cfg(all(feature = "kitty-shm", unix, not(target_os = "android")))]
fn read_shared_memory_data(
    name: &str,
    data_offset: Option<u32>,
    data_size: Option<u32>,
) -> std::result::Result<std::vec::Vec<u8>, std::io::Error> {
    use nix::sys::mman::{shm_open, shm_unlink};
    use std::fs::File;
    use std::io::Seek;

    let fd = shm_open(
        name,
        nix::fcntl::OFlag::O_RDONLY,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|_| {
        let err = std::io::Error::last_os_error();
        std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("shm_open {} failed: {:#}", name, err),
        )
    })?;
    // Unlink immediately: the name disappears but the object lives on until
    // the descriptor closes, so the reads below still work — and every exit,
    // a seek error or a refused oversized payload included, cleans up rather
    // than leaking the object.
    if let Err(err) = shm_unlink(name) {
        log::warn!(
            "Unable to unlink kitty image protocol shm file {}: {:#}",
            name,
            err
        );
    }

    let mut f = File::from(fd);
    let offset = u64::from(data_offset.unwrap_or(0));
    let available = f
        .metadata()
        .ok()
        .map(|metadata| metadata.len().saturating_sub(offset));
    if offset != 0 {
        f.seek(std::io::SeekFrom::Start(offset))?;
    }
    let data = if let Some(len) = data_size {
        read_exactly(
            &mut f,
            len,
            MAX_IMAGE_DATA_BYTES,
            available.map(|available| available.min(u64::from(len))),
        )?
    } else {
        read_to_end_capped(&mut f, MAX_IMAGE_DATA_BYTES, available)?
    };

    Ok(data)
}

// Android has the API surface but not the permission to use it, and targets
// like wasm32 have no shared memory at all. Both answer the same way, so the
// parser keeps understanding the escape sequence and simply declines the
// transfer rather than failing to build.
#[cfg(all(
    feature = "kitty-shm",
    not(windows),
    any(not(unix), target_os = "android")
))]
fn read_shared_memory_data(
    _name: &str,
    _data_offset: Option<u32>,
    _data_size: Option<u32>,
) -> std::result::Result<std::vec::Vec<u8>, std::io::Error> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(all(feature = "kitty-shm", windows))]
mod win {
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::memoryapi::{
        FILE_MAP_ALL_ACCESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery,
    };
    use winapi::um::winnt::{HANDLE, MEMORY_BASIC_INFORMATION};

    struct HandleWrapper {
        handle: HANDLE,
    }

    struct SharedMemObject {
        _handle: HandleWrapper,
        buf: *mut u8,
    }

    impl Drop for HandleWrapper {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }

    impl Drop for SharedMemObject {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(self.buf as _);
            }
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

    pub fn read_shared_memory_data(
        name: &str,
        data_offset: Option<u32>,
        data_size: Option<u32>,
    ) -> std::result::Result<std::vec::Vec<u8>, std::io::Error> {
        let wide_name = wide_string(&name);

        let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide_name.as_ptr()) };
        if handle.is_null() {
            let err = std::io::Error::last_os_error();
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("OpenFileMappingW {} failed: {:#}", name, err),
            ));
        }

        let handle_wrapper = HandleWrapper { handle };
        let buf = unsafe { MapViewOfFile(handle_wrapper.handle, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        if buf.is_null() {
            let err = std::io::Error::last_os_error();
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("MapViewOfFile failed: {:#}", err),
            ));
        }

        let shm = SharedMemObject {
            _handle: handle_wrapper,
            buf: buf as *mut u8,
        };

        let mut memory_info = MEMORY_BASIC_INFORMATION::default();
        let res = unsafe {
            VirtualQuery(
                shm.buf as _,
                &mut memory_info as *mut MEMORY_BASIC_INFORMATION,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if res == 0 {
            let err = std::io::Error::last_os_error();
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "Can't get the size of Shared Memory, VirtualQuery failed: {:#}",
                    err
                ),
            ));
        }
        let mut size = memory_info.RegionSize;
        let offset = data_offset.unwrap_or(0) as usize;
        if offset >= size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "offset {} bigger than or equal to shm region size {}",
                    offset, size
                ),
            ));
        }
        size = size.saturating_sub(offset);
        if let Some(val) = data_size {
            size = size.min(val as usize);
        }
        let buf_slice = unsafe { std::slice::from_raw_parts(shm.buf.add(offset), size) };
        let data = buf_slice.to_vec();

        Ok(data)
    }
}

#[cfg(all(feature = "kitty-shm", windows))]
use win::read_shared_memory_data;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum KittyImageVerbosity {
    Verbose,
    OnlyErrors,
    Quiet,
}

impl KittyImageVerbosity {
    fn from_keys(keys: &BTreeMap<&str, &str>) -> Option<Self> {
        match get(keys, "q") {
            None | Some("0") => Some(Self::Verbose),
            Some("1") => Some(Self::OnlyErrors),
            Some("2") => Some(Self::Quiet),
            _ => None,
        }
    }

    fn to_keys(&self, keys: &mut BTreeMap<&'static str, String>) {
        match self {
            Self::Verbose => {}
            Self::OnlyErrors => {
                keys.insert("q", "1".to_string());
            }
            Self::Quiet => {
                keys.insert("q", "2".to_string());
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyImageFormat {
    /// f=24
    Rgb,
    /// f=32
    Rgba,
    /// f=100
    Png,
}

impl KittyImageFormat {
    fn from_keys(keys: &BTreeMap<&str, &str>) -> Option<Option<Self>> {
        match get(keys, "f") {
            None => Some(None),
            Some("32") => Some(Some(Self::Rgba)),
            Some("24") => Some(Some(Self::Rgb)),
            Some("100") => Some(Some(Self::Png)),
            _ => None,
        }
    }

    fn to_keys(&self, keys: &mut BTreeMap<&'static str, String>) {
        match self {
            Self::Rgb => keys.insert("f", "24".to_string()),
            Self::Rgba => keys.insert("f", "32".to_string()),
            Self::Png => keys.insert("f", "100".to_string()),
        };
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyImageCompression {
    None,
    /// o='z'
    Deflate,
}

impl KittyImageCompression {
    fn from_keys(keys: &BTreeMap<&str, &str>) -> Option<Self> {
        match get(keys, "o") {
            None => Some(Self::None),
            Some("z") => Some(Self::Deflate),
            _ => None,
        }
    }

    fn to_keys(&self, keys: &mut BTreeMap<&'static str, String>) {
        match self {
            Self::None => {}
            Self::Deflate => {
                keys.insert("o", "z".to_string());
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyImageTransmit {
    /// f=...
    pub format: Option<KittyImageFormat>,
    /// combination of t=... and d=...
    pub data: KittyImageData,
    /// s=...
    pub width: Option<u32>,
    /// v=...
    pub height: Option<u32>,
    /// The image id.
    /// i=...
    pub image_id: Option<u32>,
    /// The image number
    /// I=...
    pub image_number: Option<u32>,
    /// o=...
    pub compression: KittyImageCompression,

    /// m=0 or m=1
    pub more_data_follows: bool,
}

impl KittyImageTransmit {
    fn from_keys(keys: &BTreeMap<&str, &str>, payload: &[u8]) -> Option<Self> {
        Some(Self {
            format: KittyImageFormat::from_keys(keys)?,
            data: KittyImageData::from_keys(keys, payload)?,
            compression: KittyImageCompression::from_keys(keys)?,
            width: geti(keys, "s"),
            height: geti(keys, "v"),
            image_id: geti(keys, "i"),
            image_number: geti(keys, "I"),
            more_data_follows: match get(keys, "m") {
                None | Some("0") => false,
                Some("1") => true,
                _ => return None,
            },
        })
    }

    fn to_keys(&self, keys: &mut BTreeMap<&'static str, String>) {
        if let Some(f) = &self.format {
            f.to_keys(keys);
        }

        set(keys, "s", &self.width);
        set(keys, "v", &self.height);
        set(keys, "i", &self.image_id);
        set(keys, "I", &self.image_number);
        if self.more_data_follows {
            keys.insert("m", "1".to_string());
        }

        self.compression.to_keys(keys);
        self.data.to_keys(keys);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyImagePlacement {
    /// source rectangle bounds.
    /// Default is whole image.
    /// x=...
    pub x: Option<u32>,
    pub y: Option<u32>,
    pub w: Option<u32>,
    pub h: Option<u32>,
    /// Place the image at an offset from the cell.
    /// X,Y must be <= cell metrics
    /// X=...
    pub x_offset: Option<u32>,
    /// Y=...
    pub y_offset: Option<u32>,
    /// Scale so that the image fits within this number of columns
    /// c=...
    pub columns: Option<u32>,
    /// Scale so that the image fits within this number of rows
    /// r=...
    pub rows: Option<u32>,
    /// By default, cursor will move to after the bottom right
    /// cell of the image placement.  do_not_move_cursor cursor
    /// set to true prevents that.
    /// C=0, C=1
    pub do_not_move_cursor: bool,
    /// Give an explicit placement id to this placement.
    /// p=...
    pub placement_id: Option<u32>,
    /// z=...
    pub z_index: Option<i32>,
    /// A virtual placement draws nothing by itself. It registers that the
    /// image is ready, and the application then prints U+10EEEE placeholder
    /// cells to say where it should appear.
    /// U=0, U=1
    pub virtual_placement: bool,
}

impl KittyImagePlacement {
    fn from_keys(keys: &BTreeMap<&str, &str>) -> Option<Self> {
        Some(Self {
            x: geti(keys, "x"),
            y: geti(keys, "y"),
            w: geti(keys, "w"),
            h: geti(keys, "h"),
            x_offset: geti(keys, "X"),
            y_offset: geti(keys, "Y"),
            columns: geti(keys, "c"),
            rows: geti(keys, "r"),
            placement_id: geti(keys, "p"),
            do_not_move_cursor: match get(keys, "C") {
                None | Some("0") => false,
                Some("1") => true,
                _ => return None,
            },
            z_index: geti(keys, "z"),
            virtual_placement: match get(keys, "U") {
                None | Some("0") => false,
                Some("1") => true,
                _ => return None,
            },
        })
    }

    fn to_keys(&self, keys: &mut BTreeMap<&'static str, String>) {
        set(keys, "x", &self.x);
        set(keys, "y", &self.y);
        set(keys, "w", &self.w);
        set(keys, "h", &self.h);
        set(keys, "X", &self.x_offset);
        set(keys, "Y", &self.y_offset);
        set(keys, "c", &self.columns);
        set(keys, "r", &self.rows);
        set(keys, "p", &self.placement_id);

        if self.do_not_move_cursor {
            keys.insert("C", "1".to_string());
        }

        if self.virtual_placement {
            keys.insert("U", "1".to_string());
        }

        set(keys, "z", &self.z_index);
    }
}

/// When the uppercase form is used, the delete: field is set to true
/// which means that the underlying data is also released.  Otherwise,
/// the data is available to be placed again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyImageDelete {
    /// d='a' or d='A'.
    /// Delete all placements on visible screen
    All { delete: bool },
    /// d='i' or d='I'
    /// Delete all images with specified image_id.
    /// If placement_id is specified, then both image_id
    /// and placement_id must match
    ByImageId {
        image_id: u32,
        placement_id: Option<u32>,
        delete: bool,
    },
    /// d='n' or d='N'
    /// Delete newest image with specified image number.
    /// If placement_id is specified, then placement_id
    /// must also match.
    ByImageNumber {
        image_number: u32,
        placement_id: Option<u32>,
        delete: bool,
    },

    /// d='c' or d='C'
    /// Delete all placements that intersect with the current
    /// cursor position.
    AtCursorPosition { delete: bool },

    /// d='f' or d='F'
    /// Delete animation frames
    AnimationFrames { delete: bool },

    /// d='p' or d='P'
    /// Delete all placements that intersect the specified
    /// cell x and y coordinates
    DeleteAt { x: u32, y: u32, delete: bool },

    /// d='q' or d='Q'
    /// Delete all placements that intersect the specified
    /// cell x and y coordinates, with the specified z-index
    DeleteAtZ {
        x: u32,
        y: u32,
        z: i32,
        delete: bool,
    },

    /// d='x' or d='X'
    /// Delete all placements that intersect the specified column.
    DeleteColumn { x: u32, delete: bool },

    /// d='y' or d='Y'
    /// Delete all placements that intersect the specified row.
    DeleteRow { y: u32, delete: bool },

    /// d='z' or d='Z'
    /// Delete all placements that have the specified z-index.
    DeleteZ { z: i32, delete: bool },
}

impl KittyImageDelete {
    fn from_keys(keys: &BTreeMap<&str, &str>) -> Option<Self> {
        let d = get(keys, "d").unwrap_or("a");
        if d.len() != 1 {
            return None;
        }
        let d = d.chars().next()?;
        let delete = d.is_ascii_uppercase();
        match d {
            'a' | 'A' => Some(Self::All { delete }),
            'i' | 'I' => Some(Self::ByImageId {
                image_id: geti(keys, "i")?,
                placement_id: geti(keys, "p"),
                delete,
            }),
            'n' | 'N' => Some(Self::ByImageNumber {
                image_number: geti(keys, "I")?,
                placement_id: geti(keys, "p"),
                delete,
            }),
            'c' | 'C' => Some(Self::AtCursorPosition { delete }),
            'f' | 'F' => Some(Self::AnimationFrames { delete }),
            'p' | 'P' => Some(Self::DeleteAt {
                x: geti(keys, "x")?,
                y: geti(keys, "y")?,
                delete,
            }),
            'q' | 'Q' => Some(Self::DeleteAtZ {
                x: geti(keys, "x")?,
                y: geti(keys, "y")?,
                z: geti(keys, "z")?,
                delete,
            }),
            'x' | 'X' => Some(Self::DeleteColumn {
                x: geti(keys, "x")?,
                delete,
            }),
            'y' | 'Y' => Some(Self::DeleteRow {
                y: geti(keys, "y")?,
                delete,
            }),
            'z' | 'Z' => Some(Self::DeleteZ {
                z: geti(keys, "z")?,
                delete,
            }),
            _ => None,
        }
    }

    fn to_keys(&self, keys: &mut BTreeMap<&'static str, String>) {
        fn d(c: char, delete: &bool) -> String {
            if *delete { c.to_ascii_uppercase() } else { c }.to_string()
        }

        match self {
            Self::All { delete } => {
                keys.insert("d", d('a', delete));
            }
            Self::ByImageId {
                image_id,
                placement_id,
                delete,
            } => {
                keys.insert("d", d('i', delete));
                if let Some(p) = placement_id {
                    keys.insert("p", p.to_string());
                }
                keys.insert("i", image_id.to_string());
            }
            Self::ByImageNumber {
                image_number,
                placement_id,
                delete,
            } => {
                keys.insert("d", d('n', delete));
                if let Some(p) = placement_id {
                    keys.insert("p", p.to_string());
                }
                keys.insert("I", image_number.to_string());
            }
            Self::AtCursorPosition { delete } => {
                keys.insert("d", d('c', delete));
            }
            Self::AnimationFrames { delete } => {
                keys.insert("d", d('f', delete));
            }
            Self::DeleteAt { x, y, delete } => {
                keys.insert("d", d('p', delete));
                keys.insert("x", x.to_string());
                keys.insert("y", y.to_string());
            }
            Self::DeleteAtZ { x, y, z, delete } => {
                keys.insert("d", d('p', delete));
                keys.insert("x", x.to_string());
                keys.insert("y", y.to_string());
                keys.insert("z", z.to_string());
            }
            Self::DeleteColumn { x, delete } => {
                keys.insert("d", d('x', delete));
                keys.insert("x", x.to_string());
            }
            Self::DeleteRow { y, delete } => {
                keys.insert("d", d('y', delete));
                keys.insert("y", y.to_string());
            }
            Self::DeleteZ { z, delete } => {
                keys.insert("d", d('z', delete));
                keys.insert("z", z.to_string());
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyFrameCompositionMode {
    AlphaBlending,
    Overwrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyImageFrameCompose {
    /// i=...
    pub image_id: Option<u32>,
    /// I=...
    pub image_number: Option<u32>,

    /// 1-based number of the frame which should be the base
    /// data for the new frame being created.
    /// If omitted, use background_pixel to specify color.
    /// c=...
    pub target_frame: Option<u32>,

    /// 1-based number of the frame which should be edited.
    /// If omitted, a new frame is created.
    /// r=...
    pub source_frame: Option<u32>,

    /// Left edge in pixels to update
    /// x=...
    pub x: Option<u32>,
    /// Top edge in pixels to update
    /// y=...
    pub y: Option<u32>,

    /// Width (in pixels) of the source and destination rectangles.
    /// By default the full width is used.
    /// w=...
    pub w: Option<u32>,

    /// Height (in pixels) of the source and destination rectangles.
    /// By default the full height is used.
    /// h=...
    pub h: Option<u32>,

    /// Left edge in pixels of the source rectangle
    /// X=...
    pub src_x: Option<u32>,
    /// Top edge in pixels of the source rectangle
    /// Y=...
    pub src_y: Option<u32>,

    /// Composition mode.
    /// Default is AlphaBlending
    /// C=...
    pub composition_mode: KittyFrameCompositionMode,
}

impl KittyImageFrameCompose {
    fn from_keys(keys: &BTreeMap<&str, &str>) -> Option<Self> {
        Some(Self {
            image_id: geti(keys, "i"),
            image_number: geti(keys, "I"),
            x: geti(keys, "x"),
            y: geti(keys, "y"),
            src_x: geti(keys, "X"),
            src_y: geti(keys, "Y"),
            w: geti(keys, "w"),
            h: geti(keys, "h"),
            target_frame: match geti(keys, "c") {
                None | Some(0) => None,
                n => n,
            },
            source_frame: match geti(keys, "r") {
                None | Some(0) => None,
                n => n,
            },
            composition_mode: match geti(keys, "C") {
                None | Some(0) => KittyFrameCompositionMode::AlphaBlending,
                Some(1) => KittyFrameCompositionMode::Overwrite,
                _ => return None,
            },
        })
    }

    fn to_keys(&self, keys: &mut BTreeMap<&'static str, String>) {
        set(keys, "i", &self.image_id);
        set(keys, "I", &self.image_number);
        set(keys, "w", &self.w);
        set(keys, "h", &self.h);
        set(keys, "x", &self.x);
        set(keys, "y", &self.y);
        set(keys, "X", &self.src_x);
        set(keys, "Y", &self.src_y);
        set(keys, "c", &self.target_frame);
        set(keys, "r", &self.source_frame);
        match &self.composition_mode {
            KittyFrameCompositionMode::AlphaBlending => {}
            KittyFrameCompositionMode::Overwrite => {
                keys.insert("C", "1".to_string());
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyImageFrame {
    /// Left edge in pixels to update
    pub x: Option<u32>,
    /// Top edge in pixels to update
    pub y: Option<u32>,

    /// 1-based number of the frame which should be the base
    /// data for the new frame being created.
    /// If omitted, use background_pixel to specify color.
    /// c=...
    pub base_frame: Option<u32>,

    /// 1-based number of the frame which should be edited.
    /// If omitted, a new frame is created.
    /// r=...
    pub frame_number: Option<u32>,

    /// Gap in milliseconds of this frame from the next one.
    /// Zero or omitted values are interpreted as 40ms.
    /// z=...
    pub duration_ms: Option<u32>,

    /// Composition mode.
    /// Default is AlphaBlending
    /// X=...
    pub composition_mode: KittyFrameCompositionMode,

    /// Background color for pixels not specified in the frame data.
    /// If omitted, use a black, fully-transparent pixel (0)
    /// Y=...
    pub background_pixel: Option<u32>,
}

impl KittyImageFrame {
    fn from_keys(keys: &BTreeMap<&str, &str>) -> Option<Self> {
        Some(Self {
            x: geti(keys, "x"),
            y: geti(keys, "y"),
            base_frame: match geti(keys, "c") {
                None | Some(0) => None,
                n => n,
            },
            frame_number: match geti(keys, "r") {
                None | Some(0) => None,
                n => n,
            },
            duration_ms: match geti(keys, "z") {
                None | Some(0) => None,
                n => n,
            },
            composition_mode: match geti(keys, "X") {
                None | Some(0) => KittyFrameCompositionMode::AlphaBlending,
                Some(1) => KittyFrameCompositionMode::Overwrite,
                _ => return None,
            },
            background_pixel: geti(keys, "Y"),
        })
    }

    fn to_keys(&self, keys: &mut BTreeMap<&'static str, String>) {
        set(keys, "x", &self.x);
        set(keys, "y", &self.y);
        set(keys, "c", &self.base_frame);
        set(keys, "r", &self.frame_number);
        set(keys, "z", &self.duration_ms);
        match &self.composition_mode {
            KittyFrameCompositionMode::AlphaBlending => {}
            KittyFrameCompositionMode::Overwrite => {
                keys.insert("X", "1".to_string());
            }
        }
        set(keys, "Y", &self.background_pixel);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyImage {
    /// a='t'
    TransmitData {
        transmit: KittyImageTransmit,
        verbosity: KittyImageVerbosity,
    },
    /// a='T'
    TransmitDataAndDisplay {
        transmit: KittyImageTransmit,
        placement: KittyImagePlacement,
        verbosity: KittyImageVerbosity,
    },
    /// a='p'
    Display {
        image_id: Option<u32>,
        image_number: Option<u32>,
        placement: KittyImagePlacement,
        verbosity: KittyImageVerbosity,
    },
    /// a='d'
    Delete {
        what: KittyImageDelete,
        verbosity: KittyImageVerbosity,
    },
    /// a='q'
    Query {
        transmit: KittyImageTransmit,
        verbosity: KittyImageVerbosity,
    },
    /// a='f'
    TransmitFrame {
        transmit: KittyImageTransmit,
        frame: KittyImageFrame,
        verbosity: KittyImageVerbosity,
    },
    /// a='c'
    ComposeFrame {
        frame: KittyImageFrameCompose,
        verbosity: KittyImageVerbosity,
    },
}

impl KittyImage {
    /// Whether this action names a file or shared-memory source that would do
    /// blocking IO if the terminal processed it directly.
    pub fn has_external_data_source(&self) -> bool {
        #[cfg(feature = "kitty-shm")]
        {
            let data = match self {
                Self::TransmitData { transmit, .. }
                | Self::TransmitDataAndDisplay { transmit, .. }
                | Self::Query { transmit, .. }
                | Self::TransmitFrame { transmit, .. } => &transmit.data,
                Self::Display { .. } | Self::Delete { .. } | Self::ComposeFrame { .. } => {
                    return false;
                }
            };
            matches!(
                data,
                KittyImageData::File { .. }
                    | KittyImageData::TemporaryFile { .. }
                    | KittyImageData::SharedMem { .. }
            )
        }
        #[cfg(not(feature = "kitty-shm"))]
        {
            false
        }
    }

    /// See [`KittyImageData::discard_external_source`].
    /// The image id a transmit replaces, when it names one. Only `a=t` and
    /// `a=T` with `i=` replace an image: `I=` (image number) allocates a
    /// fresh id per transmit, an id-less transmit is its own picture, a
    /// frame (`a=f`) extends an animation rather than replacing it, and a
    /// query stores nothing. A chunked transmit (`m=1`) has not replaced
    /// anything yet -- its tail may never arrive -- so it must not count
    /// as newer than a complete frame under the same id.
    pub fn replacing_transmit_image_id(&self) -> Option<u32> {
        match self {
            Self::TransmitData { transmit, .. } | Self::TransmitDataAndDisplay { transmit, .. } => {
                if transmit.more_data_follows {
                    return None;
                }
                transmit.image_id
            }
            Self::Query { .. }
            | Self::TransmitFrame { .. }
            | Self::Display { .. }
            | Self::Delete { .. }
            | Self::ComposeFrame { .. } => None,
        }
    }

    pub fn discard_data_sources(&mut self, reason: &str) {
        #[cfg(feature = "kitty-shm")]
        match self {
            Self::TransmitData { transmit, .. }
            | Self::TransmitDataAndDisplay { transmit, .. }
            | Self::Query { transmit, .. }
            | Self::TransmitFrame { transmit, .. } => {
                transmit.data.discard_external_source(reason);
            }
            Self::Display { .. } | Self::Delete { .. } | Self::ComposeFrame { .. } => {}
        }
        #[cfg(not(feature = "kitty-shm"))]
        {
            let _ = reason;
        }
    }

    /// Bytes an already materialized payload holds in memory.
    pub fn materialized_len(&self) -> usize {
        #[cfg(feature = "kitty-shm")]
        {
            match self {
                Self::TransmitData { transmit, .. }
                | Self::TransmitDataAndDisplay { transmit, .. }
                | Self::Query { transmit, .. }
                | Self::TransmitFrame { transmit, .. } => transmit.data.materialized_len(),
                Self::Display { .. } | Self::Delete { .. } | Self::ComposeFrame { .. } => 0,
            }
        }
        #[cfg(not(feature = "kitty-shm"))]
        {
            0
        }
    }

    /// Read external payloads before the action enters the terminal model.
    /// Direct fragments stay encoded until the terminal applies its ordering
    /// and accumulation rules.
    pub fn materialize_data_sources(&mut self) {
        #[cfg(feature = "kitty-shm")]
        match self {
            Self::TransmitData { transmit, .. }
            | Self::TransmitDataAndDisplay { transmit, .. }
            | Self::Query { transmit, .. }
            | Self::TransmitFrame { transmit, .. } => {
                transmit.data.materialize_external_source();
            }
            Self::Display { .. } | Self::Delete { .. } | Self::ComposeFrame { .. } => {}
        }
    }

    pub fn verbosity(&self) -> KittyImageVerbosity {
        match self {
            Self::TransmitData { verbosity, .. } => *verbosity,
            Self::Query { verbosity, .. } => *verbosity,
            Self::TransmitDataAndDisplay { verbosity, .. } => *verbosity,
            Self::Display { verbosity, .. } => *verbosity,
            Self::Delete { verbosity, .. } => *verbosity,
            Self::TransmitFrame { verbosity, .. } => *verbosity,
            Self::ComposeFrame { verbosity, .. } => *verbosity,
        }
    }

    pub fn parse_apc(data: &[u8]) -> Option<Self> {
        if data.is_empty() || data[0] != b'G' {
            return None;
        }
        let mut keys_payload_iter = data[1..].splitn(2, |&d| d == b';');
        let keys = keys_payload_iter.next()?;
        let key_string = core::str::from_utf8(keys).ok()?;
        let mut keys: BTreeMap<&str, &str> = BTreeMap::new();
        for k_v in key_string.split(',') {
            let mut k_v = k_v.splitn(2, '=');
            let k = k_v.next()?;
            let v = k_v.next()?;
            keys.insert(k, v);
        }

        let payload = keys_payload_iter.next().unwrap_or(b"");
        let action = get(&keys, "a").unwrap_or("t");
        let verbosity = KittyImageVerbosity::from_keys(&keys)?;
        match action {
            "t" => Some(Self::TransmitData {
                transmit: KittyImageTransmit::from_keys(&keys, payload)?,
                verbosity,
            }),
            "q" => Some(Self::Query {
                transmit: KittyImageTransmit::from_keys(&keys, payload)?,
                verbosity,
            }),
            "T" => Some(Self::TransmitDataAndDisplay {
                transmit: KittyImageTransmit::from_keys(&keys, payload)?,
                placement: KittyImagePlacement::from_keys(&keys)?,
                verbosity,
            }),
            "p" => Some(Self::Display {
                placement: KittyImagePlacement::from_keys(&keys)?,
                image_id: geti(&keys, "i"),
                image_number: geti(&keys, "I"),
                verbosity,
            }),
            "d" => Some(Self::Delete {
                what: KittyImageDelete::from_keys(&keys)?,
                verbosity,
            }),
            "f" => Some(Self::TransmitFrame {
                transmit: KittyImageTransmit::from_keys(&keys, payload)?,
                frame: KittyImageFrame::from_keys(&keys)?,
                verbosity,
            }),
            "c" => Some(Self::ComposeFrame {
                frame: KittyImageFrameCompose::from_keys(&keys)?,
                verbosity,
            }),
            _ => None,
        }
    }

    fn to_keys(&self, keys: &mut BTreeMap<&'static str, String>) {
        match self {
            Self::TransmitData {
                transmit,
                verbosity,
            } => {
                // Implied: keys.insert("a", "t".to_string());
                verbosity.to_keys(keys);
                transmit.to_keys(keys);
            }
            Self::Query {
                transmit,
                verbosity,
            } => {
                keys.insert("a", "q".to_string());
                verbosity.to_keys(keys);
                transmit.to_keys(keys);
            }
            Self::TransmitDataAndDisplay {
                transmit,
                verbosity,
                placement,
            } => {
                keys.insert("a", "Q".to_string());
                verbosity.to_keys(keys);
                placement.to_keys(keys);
                transmit.to_keys(keys);
            }
            Self::Display {
                image_id,
                image_number,
                placement,
                verbosity,
            } => {
                keys.insert("a", "p".to_string());
                verbosity.to_keys(keys);
                placement.to_keys(keys);
                if let Some(image_id) = image_id {
                    keys.insert("i", image_id.to_string());
                }
                if let Some(image_number) = image_number {
                    keys.insert("I", image_number.to_string());
                }
            }
            Self::Delete { what, verbosity } => {
                keys.insert("a", "d".to_string());
                verbosity.to_keys(keys);
                what.to_keys(keys);
            }
            Self::TransmitFrame {
                transmit,
                verbosity,
                frame,
            } => {
                keys.insert("a", "f".to_string());
                transmit.to_keys(keys);
                frame.to_keys(keys);
                verbosity.to_keys(keys);
            }
            Self::ComposeFrame { frame, verbosity } => {
                keys.insert("a", "c".to_string());
                frame.to_keys(keys);
                verbosity.to_keys(keys);
            }
        }
    }
}

impl Display for KittyImage {
    fn fmt(&self, f: &mut Formatter) -> Result<(), FmtError> {
        write!(f, "\x1b_G")?;
        let mut keys = BTreeMap::new();
        self.to_keys(&mut keys);
        let mut payload = None;
        let mut first = true;
        for (k, v) in keys {
            if k == "payload" {
                payload = Some(v);
            } else {
                if first {
                    first = false;
                } else {
                    write!(f, ",")?;
                }

                write!(f, "{}={}", k, v)?;
            }
        }

        if let Some(p) = payload {
            write!(f, ";{}", p)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use k9::assert_equal as assert_eq;

    #[test]
    fn kitty_payload() {
        assert_eq!(
            KittyImage::parse_apc("Gf=24,s=10,v=20;aGVsbG8=".as_bytes()).unwrap(),
            KittyImage::TransmitData {
                transmit: KittyImageTransmit {
                    format: Some(KittyImageFormat::Rgb),
                    data: KittyImageData::Direct("aGVsbG8=".to_string()),
                    width: Some(10),
                    height: Some(20),
                    image_id: None,
                    image_number: None,
                    compression: KittyImageCompression::None,
                    more_data_follows: false,
                },
                verbosity: KittyImageVerbosity::Verbose,
            }
        );

        assert_eq!(
            KittyImage::parse_apc("Ga=d,q=2".as_bytes()).unwrap(),
            KittyImage::Delete {
                what: KittyImageDelete::All { delete: false },
                verbosity: KittyImageVerbosity::Quiet
            }
        );

        assert_eq!(
            KittyImage::parse_apc(
                "Ga=f,x=119,y=384,s=17,v=32,i=7257421,X=1,r=1,q=2;AAAA=".as_bytes()
            )
            .unwrap(),
            KittyImage::TransmitFrame {
                transmit: KittyImageTransmit {
                    format: None,
                    data: KittyImageData::Direct("AAAA=".to_string()),
                    width: Some(17),
                    height: Some(32),
                    image_id: Some(7257421),
                    image_number: None,
                    compression: KittyImageCompression::None,
                    more_data_follows: false,
                },
                verbosity: KittyImageVerbosity::Quiet,
                frame: KittyImageFrame {
                    x: Some(119),
                    y: Some(384),
                    base_frame: None,
                    frame_number: Some(1),
                    composition_mode: KittyFrameCompositionMode::Overwrite,
                    background_pixel: None,
                    duration_ms: None,
                },
            }
        );
    }
}

#[cfg(all(test, feature = "kitty-shm", unix, not(target_os = "android")))]
mod shm_test {
    use super::*;

    #[test]
    fn a_refused_shared_memory_payload_is_still_unlinked() {
        use nix::fcntl::OFlag;
        use nix::sys::mman::{shm_open, shm_unlink};
        use nix::sys::stat::Mode;

        let name = format!("/tt-apc-{}", std::process::id());
        // Tolerate a leftover from a crashed earlier run.
        let _ = shm_unlink(name.as_str());

        let fd = shm_open(
            name.as_str(),
            OFlag::O_CREAT | OFlag::O_RDWR | OFlag::O_EXCL,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .expect("shm_open create");
        nix::unistd::ftruncate(&fd, 64).expect("ftruncate");
        drop(fd);

        // S= far over the cap is refused before any read happens; the
        // object must be unlinked anyway, or refused transfers pile up.
        let err = KittyImageData::SharedMem {
            name: name.clone(),
            data_offset: None,
            data_size: Some(u32::MAX),
        }
        .load_data()
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

        assert!(
            shm_open(name.as_str(), OFlag::O_RDONLY, Mode::empty()).is_err(),
            "the refused shared-memory object should have been unlinked"
        );
    }
}

#[cfg(all(test, feature = "kitty-shm", unix))]
mod temp_file_test {
    use super::*;

    /// Removes its directory when it goes out of scope, so a failing
    /// assertion does not leave the tree dirty.
    struct ScratchDir(std::path::PathBuf);

    impl ScratchDir {
        /// Creates a directory that is deliberately *not* under any of the
        /// temporary roots the protocol is allowed to delete from.
        fn outside_temp(tag: &str) -> Self {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
                ".apc-test-{}-{}",
                tag,
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn in_temp(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("apc-test-{}-{}", tag, std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, name: &str, contents: &[u8]) -> std::path::PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn load_temporary_file(path: String) -> std::io::Result<Vec<u8>> {
        KittyImageData::TemporaryFile {
            path,
            data_offset: None,
            data_size: None,
        }
        .load_data()
    }

    #[test]
    fn a_real_temporary_file_is_read_and_unlinked() {
        let dir = ScratchDir::in_temp("unlink");
        let file = dir.file("frame.rgba", b"payload");

        let data = load_temporary_file(file.to_str().unwrap().to_string()).unwrap();

        assert_eq!(data, b"payload");
        assert!(
            !file.exists(),
            "a genuine temporary file should be unlinked"
        );
    }

    #[test]
    fn traversal_out_of_the_temp_dir_does_not_unlink() {
        let dir = ScratchDir::outside_temp("traversal");
        let sentinel = dir.file("sentinel", b"do not delete me");

        // Enough `..` to reach the filesystem root from either /tmp or the
        // /private/tmp that /tmp resolves to on macOS. This passes the old
        // `starts_with("/tmp/")` test while naming a file anywhere on disk.
        let traversal = format!("/tmp/../..{}", sentinel.display());

        let data = load_temporary_file(traversal).unwrap();

        assert_eq!(data, b"do not delete me");
        assert!(
            sentinel.exists(),
            "a path resolving outside the temp dirs must never be unlinked"
        );
    }

    #[test]
    fn a_symlink_pointing_out_of_the_temp_dir_does_not_unlink() {
        let outside = ScratchDir::outside_temp("symlink-target");
        let sentinel = outside.file("sentinel", b"do not delete me either");

        let temp = ScratchDir::in_temp("symlink");
        let link = temp.0.join("frame.rgba");
        std::os::unix::fs::symlink(&sentinel, &link).unwrap();

        let data = load_temporary_file(link.to_str().unwrap().to_string()).unwrap();

        assert_eq!(data, b"do not delete me either");
        assert!(
            sentinel.exists(),
            "following a symlink out of the temp dirs must not unlink its target"
        );
    }

    #[test]
    fn character_devices_are_not_read() {
        // Bounded by data_size so that a regression fails the assertion
        // instead of reading /dev/zero until the machine gives up.
        let err = KittyImageData::File {
            path: "/dev/zero".to_string(),
            data_offset: None,
            data_size: Some(16),
        }
        .load_data()
        .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn directories_are_not_read() {
        let err = KittyImageData::File {
            path: "/tmp".to_string(),
            data_offset: None,
            data_size: None,
        }
        .load_data()
        .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn an_oversized_file_read_is_refused() {
        let dir = ScratchDir::in_temp("cap");
        let big = dir.file("big", &[0u8; 32]);
        let small = dir.file("small", &[0u8; 16]);

        let err = read_from_file_capped(big.to_str().unwrap(), None, None, 16).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

        let data = read_from_file_capped(small.to_str().unwrap(), None, None, 16).unwrap();
        assert_eq!(data.len(), 16);
    }

    #[test]
    fn a_data_size_over_the_cap_is_refused_without_reading() {
        let dir = ScratchDir::in_temp("cap-size");
        let file = dir.file("f", &[0u8; 8]);

        let err = read_from_file_capped(file.to_str().unwrap(), None, Some(64), 16).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn an_offset_read_respects_the_cap() {
        let dir = ScratchDir::in_temp("cap-offset");
        let file = dir.file("f", &[0u8; 40]);

        // Only 10 bytes lie past the offset: the cap is on the bytes read,
        // not on the size of the file they come from.
        let data = read_from_file_capped(file.to_str().unwrap(), Some(30), None, 16).unwrap();
        assert_eq!(data.len(), 10);
    }

    #[test]
    fn a_failed_temporary_file_read_still_unlinks() {
        let dir = ScratchDir::in_temp("unlink-on-error");
        let file = dir.file("frame.rgba", &[0u8; 8]);

        let err = KittyImageData::TemporaryFile {
            path: file.to_str().unwrap().to_string(),
            data_offset: None,
            data_size: Some(64),
        }
        .load_data()
        .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(
            !file.exists(),
            "a temporary file must be cleaned up even when the read fails"
        );
    }

    #[test]
    fn external_data_is_materialized_once() {
        let dir = ScratchDir::in_temp("materialize");
        let file = dir.file("frame.rgba", b"payload");
        let mut data = KittyImageData::TemporaryFile {
            path: file.to_str().unwrap().to_string(),
            data_offset: None,
            data_size: None,
        };

        data.materialize_external_source();

        assert!(
            !file.exists(),
            "materialization should perform temporary-file cleanup"
        );
        assert_eq!(data.load_data().unwrap(), b"payload");
    }

    #[test]
    fn materialized_file_survives_ring_slot_reuse() {
        let dir = ScratchDir::in_temp("materialize-ring");
        let file = dir.file("frame-0.rgba", b"first frame");
        let mut data = KittyImageData::File {
            path: file.to_str().unwrap().to_string(),
            data_offset: None,
            data_size: None,
        };

        data.materialize_external_source();

        // A streaming producer may recycle a small set of paths, or remove
        // the old set after a resize, before the terminal-model queue runs.
        std::fs::write(&file, b"later frame").unwrap();
        std::fs::remove_file(&file).unwrap();

        assert_eq!(data.load_data().unwrap(), b"first frame");
    }

    #[test]
    fn materialization_preserves_a_read_error_without_retrying() {
        let dir = ScratchDir::in_temp("materialize-error");
        let file = dir.file("frame.rgba", &[0u8; 8]);
        let mut data = KittyImageData::TemporaryFile {
            path: file.to_str().unwrap().to_string(),
            data_offset: None,
            data_size: Some(64),
        };

        data.materialize_external_source();

        assert!(!file.exists());
        let err = data.load_data().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(err.to_string().contains("wanted 64 bytes"));
    }

    #[test]
    fn read_preallocation_follows_the_file_size_up_to_the_cap() {
        assert_eq!(
            initial_read_capacity(Some(3 * 1024 * 1024), 128 * 1024 * 1024),
            3 * 1024 * 1024
        );
        assert_eq!(initial_read_capacity(Some(u64::MAX), 16), 16);
        assert_eq!(initial_read_capacity(None, u64::MAX), 0);
    }

    #[test]
    fn a_batch_reads_newest_first_and_discards_the_rest_unread() {
        use crate::Action;
        let dir = ScratchDir::in_temp("newest-first");
        let mut actions: Vec<Action> = Vec::new();
        let mut files = Vec::new();
        for i in 0..3 {
            let file = dir.file(&format!("frame-{i}.rgba"), &[i as u8; 100]);
            files.push(file.clone());
            // One id for every frame, the way a frame stream transmits.
            actions.push(temp_file_transmit(&file, Some(1)));
        }

        // 150 bytes: the newest two frames (100 each) are read, the oldest
        // is discarded without a read -- but its temporary file still goes.
        let (bytes, discarded) = materialize_kitty_actions_newest_first(&mut actions, 150);
        assert_eq!((bytes, discarded), (200, 1));
        for file in &files {
            assert!(!file.exists(), "every temporary file is unlinked");
        }
        assert_eq!(transmit_payload(&actions[2]).unwrap(), vec![2u8; 100]);
        assert_eq!(transmit_payload(&actions[1]).unwrap(), vec![1u8; 100]);
        let err = transmit_payload(&actions[0]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
    }

    fn temp_file_transmit(file: &std::path::Path, image_id: Option<u32>) -> crate::Action {
        crate::Action::KittyImage(Box::new(KittyImage::TransmitData {
            transmit: KittyImageTransmit {
                format: Some(KittyImageFormat::Rgba),
                data: KittyImageData::TemporaryFile {
                    path: file.to_str().unwrap().to_string(),
                    data_offset: None,
                    data_size: None,
                },
                width: Some(5),
                height: Some(5),
                image_id,
                image_number: None,
                compression: KittyImageCompression::None,
                more_data_follows: false,
            },
            verbosity: KittyImageVerbosity::Verbose,
        }))
    }

    fn transmit_payload(action: &crate::Action) -> std::io::Result<Vec<u8>> {
        match action {
            crate::Action::KittyImage(image) => match &**image {
                KittyImage::TransmitData { transmit, .. } => transmit.data.clone().load_data(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_gallery_of_distinct_pictures_is_read_past_the_budget() {
        // Three separate pictures -- two ids and one id-less -- coalesced
        // into one flush. None replaces another, so the budget must not
        // cost the viewer any of them.
        let dir = ScratchDir::in_temp("gallery");
        let files: Vec<_> = (0..3)
            .map(|i| dir.file(&format!("pic-{i}.rgba"), &[i as u8; 100]))
            .collect();
        let mut actions = vec![
            temp_file_transmit(&files[0], Some(1)),
            temp_file_transmit(&files[1], Some(2)),
            temp_file_transmit(&files[2], None),
        ];
        let (bytes, discarded) = materialize_kitty_actions_newest_first(&mut actions, 150);
        assert_eq!((bytes, discarded), (300, 0));
        for (i, action) in actions.iter().enumerate() {
            assert_eq!(transmit_payload(action).unwrap(), vec![i as u8; 100]);
        }
    }

    #[test]
    fn only_a_newer_transmit_of_the_same_id_supersedes() {
        // id 1 twice with id 2 in between: over budget, the old id-1 frame
        // goes and the id-2 picture stays, whichever order they arrived in.
        let dir = ScratchDir::in_temp("mixed-ids");
        let files: Vec<_> = (0..3)
            .map(|i| dir.file(&format!("pic-{i}.rgba"), &[i as u8; 100]))
            .collect();
        let mut actions = vec![
            temp_file_transmit(&files[0], Some(1)),
            temp_file_transmit(&files[1], Some(2)),
            temp_file_transmit(&files[2], Some(1)),
        ];
        let (bytes, discarded) = materialize_kitty_actions_newest_first(&mut actions, 100);
        assert_eq!((bytes, discarded), (200, 1));
        assert_eq!(transmit_payload(&actions[2]).unwrap(), vec![2u8; 100]);
        assert_eq!(transmit_payload(&actions[1]).unwrap(), vec![1u8; 100]);
        assert_eq!(
            transmit_payload(&actions[0]).unwrap_err().kind(),
            std::io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn a_whole_file_read_does_not_double_its_buffer() {
        let dir = ScratchDir::in_temp("exact-capacity");
        let payload = vec![7u8; 3 * 1024 * 1024 + 123];
        let file = dir.file("frame.rgba", &payload);
        let data =
            read_from_file_capped(file.to_str().unwrap(), None, None, 64 * 1024 * 1024).unwrap();
        assert_eq!(data, payload);
        assert_eq!(
            data.capacity(),
            data.len(),
            "the buffer must be sized from the file, not grown by doubling"
        );
    }

    #[test]
    fn a_file_that_grew_after_open_is_still_read_completely() {
        // The hint is the size at open; a producer may append before the
        // read. Pass a hint smaller than the content and make sure the tail
        // arrives.
        let mut buf = std::io::Cursor::new(vec![1u8; 100]);
        let data = read_to_end_capped(&mut buf, 1000, Some(40)).unwrap();
        assert_eq!(data.len(), 100);
        let mut buf = std::io::Cursor::new(vec![1u8; 100]);
        let err = read_to_end_capped(&mut buf, 50, Some(40)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn external_source_detection_ignores_direct_payloads() {
        let mut image = KittyImage::parse_apc(b"Gf=32,s=1,v=1;AAAAAA==").unwrap();
        assert!(!image.has_external_data_source());

        let dir = ScratchDir::in_temp("external-detection");
        let file = dir.file("frame.rgba", &[0u8; 4]);
        image = KittyImage::TransmitData {
            transmit: KittyImageTransmit {
                format: Some(KittyImageFormat::Rgba),
                data: KittyImageData::File {
                    path: file.to_str().unwrap().to_string(),
                    data_offset: None,
                    data_size: Some(4),
                },
                width: Some(1),
                height: Some(1),
                image_id: None,
                image_number: None,
                compression: KittyImageCompression::None,
                more_data_follows: false,
            },
            verbosity: KittyImageVerbosity::Verbose,
        };
        assert!(image.has_external_data_source());

        image.materialize_data_sources();
        assert!(!image.has_external_data_source());
    }

    #[test]
    fn a_symlink_inside_the_temp_dir_removes_only_the_symlink() {
        let dir = ScratchDir::in_temp("symlink-inside");
        let target = dir.file("target", b"still here");
        let link = dir.0.join("frame.rgba");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let data = load_temporary_file(link.to_str().unwrap().to_string()).unwrap();

        assert_eq!(data, b"still here");
        assert!(
            std::fs::symlink_metadata(&link).is_err(),
            "the entry the client named should be removed"
        );
        assert!(
            target.exists(),
            "unlink does not follow a final symlink, so the target must survive"
        );
    }
}
