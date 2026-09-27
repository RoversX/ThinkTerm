use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::UnixStream as StreamImpl;
#[cfg(windows)]
use std::os::windows::io::{
    AsRawSocket, AsSocket, BorrowedSocket, FromRawSocket, IntoRawSocket, RawSocket,
};
use std::path::Path;
#[cfg(windows)]
use uds_windows::UnixStream as StreamImpl;

#[cfg(unix)]
use std::os::unix::net::UnixListener as ListenerImpl;
#[cfg(windows)]
use uds_windows::UnixListener as ListenerImpl;

#[cfg(unix)]
use std::os::unix::net::SocketAddr;
#[cfg(windows)]
use uds_windows::SocketAddr;

/// This wrapper makes UnixStream IoSafe on all platforms.
/// This isn't strictly needed on unix, because async-io
/// includes an impl for the std UnixStream, but on Windows
/// the uds_windows crate doesn't have an impl.
/// Here we define it for all platforms in the interest of
/// minimizing platform differences.
#[derive(Debug)]
pub struct UnixStream(StreamImpl);

#[cfg(unix)]
impl AsFd for UnixStream {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
#[cfg(unix)]
impl IntoRawFd for UnixStream {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}
#[cfg(unix)]
impl FromRawFd for UnixStream {
    unsafe fn from_raw_fd(fd: RawFd) -> UnixStream {
        UnixStream(StreamImpl::from_raw_fd(fd))
    }
}
#[cfg(unix)]
impl AsRawFd for UnixStream {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(windows)]
impl IntoRawSocket for UnixStream {
    fn into_raw_socket(self) -> RawSocket {
        self.0.into_raw_socket()
    }
}
#[cfg(windows)]
impl AsRawSocket for UnixStream {
    fn as_raw_socket(&self) -> RawSocket {
        self.0.as_raw_socket()
    }
}
#[cfg(windows)]
impl AsSocket for UnixStream {
    fn as_socket(&self) -> BorrowedSocket {
        self.0.as_socket()
    }
}
#[cfg(windows)]
impl FromRawSocket for UnixStream {
    unsafe fn from_raw_socket(socket: RawSocket) -> UnixStream {
        UnixStream(StreamImpl::from_raw_socket(socket))
    }
}

impl Read for UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        self.0.read(buf)
    }
}

impl Write for UnixStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> Result<(), std::io::Error> {
        self.0.flush()
    }
}

unsafe impl async_io::IoSafe for UnixStream {}

impl UnixStream {
    pub fn connect<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        Ok(Self(StreamImpl::connect(path)?))
    }

    /// A second handle on the same socket, wrapped like the first, so one
    /// thread can read while another writes.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self(self.0.try_clone()?))
    }
}

impl std::ops::Deref for UnixStream {
    type Target = StreamImpl;
    fn deref(&self) -> &StreamImpl {
        &self.0
    }
}

impl std::ops::DerefMut for UnixStream {
    fn deref_mut(&mut self) -> &mut StreamImpl {
        &mut self.0
    }
}

pub struct UnixListener(ListenerImpl);

#[cfg(unix)]
impl FromRawFd for UnixListener {
    unsafe fn from_raw_fd(fd: RawFd) -> UnixListener {
        UnixListener(ListenerImpl::from_raw_fd(fd))
    }
}

impl UnixListener {
    pub fn bind<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        Ok(Self(ListenerImpl::bind(path)?))
    }

    pub fn accept(&self) -> std::io::Result<(UnixStream, SocketAddr)> {
        let (stream, addr) = self.0.accept()?;
        Ok((UnixStream(stream), addr))
    }

    pub fn incoming(&self) -> impl Iterator<Item = std::io::Result<UnixStream>> + '_ {
        self.0.incoming().map(|r| r.map(UnixStream))
    }
}

impl std::ops::Deref for UnixListener {
    type Target = ListenerImpl;
    fn deref(&self) -> &ListenerImpl {
        &self.0
    }
}

impl std::ops::DerefMut for UnixListener {
    fn deref_mut(&mut self) -> &mut ListenerImpl {
        &mut self.0
    }
}

/// Whether a listener answers on the socket at `path`.
///
/// Windows only. The unix side asks this question in two places with two
/// different answers for "the probe itself failed", so each keeps its own
/// `sockaddr_un` version; there is one Windows implementation because
/// `uds_windows` gives no way to spell those differences anyway.
///
/// Windows answers a connect to a stale socket file and to a path that does
/// not exist with the same `WSAECONNREFUSED`, so that error is the only one
/// that means nobody is there. Every other failure counts as someone,
/// because binding over a live server is the outcome this exists to
/// prevent. A listener whose accept backlog is full also answers
/// `WSAECONNREFUSED` and will be read as absent; that is the same gap the
/// unix daemon path closes with a pid file, and Windows has none.
#[cfg(windows)]
pub fn someone_listens(path: &Path) -> bool {
    const WSAECONNREFUSED: i32 = 10061;
    match UnixStream::connect(path) {
        Ok(_) => true,
        Err(err) => err.raw_os_error() != Some(WSAECONNREFUSED),
    }
}

#[cfg(all(test, windows))]
mod tests {
    /// A live listener answers; the file it leaves behind does not. The
    /// second half is what makes the probe usable: Windows keeps the socket
    /// file after the listener is gone, so its presence proves nothing.
    #[test]
    fn only_a_live_listener_answers() {
        let path = std::env::temp_dir()
            .join(format!("wezterm-uds-probe-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(
            !super::someone_listens(&path),
            "nothing is bound at {}",
            path.display()
        );

        let listener = super::UnixListener::bind(&path).unwrap();
        assert!(super::someone_listens(&path), "the listener must answer");

        drop(listener);
        assert!(path.exists(), "windows leaves the socket file behind");
        assert!(
            !super::someone_listens(&path),
            "a stale file must not read as a listener"
        );
        let _ = std::fs::remove_file(&path);
    }
}
